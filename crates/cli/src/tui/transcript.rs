//! 会话流投影：typed 事件合并为可读条目，并按宽度计算可视行。
//!
//! [`Transcript`] 是纯状态对象：assistant 增量累积成段落；工具调用以
//! [`ToolItem`] 为单位就地刷新（运行中更新预览，结束后固化为稳定记录），
//! 不向会话流追加重复行。可视行计算覆盖显式换行、CJK 宽字符与长行折行。

use super::view::truncate_label;
use super::wrapped_lines;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::cell::RefCell;
use std::collections::HashMap;

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
    /// 本地回显的用户消息；与 assistant 文本（默认前景色）区分。
    User,
}

impl NoteStyle {
    fn style(self) -> Style {
        match self {
            Self::Dim => Style::new().fg(Color::DarkGray),
            Self::Info => Style::new(),
            Self::Warning => Style::new().fg(Color::Yellow),
            Self::Error => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            Self::Accent => Style::new().fg(Color::Cyan),
            Self::User => Style::new().bg(Color::DarkGray),
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

/// 每条目物化行的缓存：宽度或 thinking 折叠态变化时全量失效，单条目
/// 就地变更按索引失效。静态条目（文本/思考/已定型的工具）跨帧复用，
/// 避免长会话每帧对全部条目重算折行。
#[derive(Debug, Default)]
struct RowCache {
    width: u16,
    thinking_collapsed: bool,
    /// 与 `Transcript::items` 平行；`None` 表示该条目未缓存。
    rows: Vec<Option<Vec<Line<'static>>>>,
}

impl RowCache {
    /// 宽度或 thinking 折叠态与上次不一致时全量失效。
    fn ensure_width(&mut self, width: u16, thinking_collapsed: bool) {
        if self.width != width || self.thinking_collapsed != thinking_collapsed {
            self.width = width;
            self.thinking_collapsed = thinking_collapsed;
            self.rows.iter_mut().for_each(|slot| *slot = None);
        }
    }

    fn invalidate(&mut self, index: usize) {
        if let Some(slot) = self.rows.get_mut(index) {
            *slot = None;
        }
    }

    fn invalidate_all(&mut self) {
        self.rows.iter_mut().for_each(|slot| *slot = None);
    }
}

/// 该条目的渲染是否随帧变化（运行中工具的光标字符、定型工具的完成
/// 闪烁）：是则实时物化，不进入缓存。
fn item_is_frame_variant(item: &FlowItem) -> bool {
    match item {
        FlowItem::Tool(tool) => match &tool.state {
            ToolState::Running { .. } => true,
            ToolState::Done { completed_at, .. } => completed_at.elapsed() < TOOL_COMPLETION_FLASH,
        },
        _ => false,
    }
}

/// 进行中 assistant 段落的折行备忘：宽度命中且缓冲只增不减时，只重包
/// 新增后缀（贪心折行从上次末行行首续算，结果与全量重包一致）；宽度变化、
/// 新段落或落定时整体重算。`consumed`/`last_start` 均为缓冲的字节偏移
///（行边界恒为字符边界，切片安全）。
#[derive(Debug, Default)]
struct LiveWrap {
    width: u16,
    lines: Vec<String>,
    consumed: usize,
    last_start: usize,
}

/// 主会话流投影状态。
#[derive(Default)]
pub(crate) struct Transcript {
    items: Vec<FlowItem>,
    /// `call_id → items 下标`：条目只追加不删除，下标稳定，工具高频更新
    /// 不再线性扫描。
    tool_index: HashMap<String, usize>,
    assistant_buffer: String,
    assistant_active: bool,
    thinking_collapsed: bool,
    row_cache: RefCell<RowCache>,
    /// 最近一次工具定型时刻：完成闪烁窗口内的帧仍需重绘（事件循环跳帧判断）。
    last_completed_at: Option<std::time::Instant>,
    live_wrap: RefCell<Option<LiveWrap>>,
}

impl Transcript {
    pub fn new() -> Self {
        // 思考默认折叠（Ctrl+T 展开）：实时思考与历史重放都不刷屏。
        Self {
            thinking_collapsed: true,
            ..Self::default()
        }
    }

    /// 统一追加点：条目入列时同步扩展缓存槽位；工具条目登记下标索引。
    fn push_item(&mut self, item: FlowItem) {
        if let FlowItem::Tool(tool) = &item {
            self.tool_index
                .entry(tool.call_id.clone())
                .or_insert(self.items.len());
        }
        self.row_cache.borrow_mut().rows.push(None);
        self.items.push(item);
    }

    /// 追加一条非流式文本（先落定进行中的 assistant 段落）。
    pub fn push_note(&mut self, text: impl Into<String>, style: NoteStyle) {
        self.flush_assistant();
        self.push_item(FlowItem::Text {
            style,
            text: text.into(),
        });
    }

    /// 本地回显一条已提交的用户消息（独立样式，区别于 assistant 文本）。
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.flush_assistant();
        self.push_item(FlowItem::Text {
            style: NoteStyle::User,
            text: text.into(),
        });
    }

    /// 累积 assistant 增量：同一段落只产生一个会话流条目。
    pub fn assistant_delta(&mut self, delta: &str) {
        if !self.assistant_active {
            self.assistant_buffer.clear();
            self.assistant_active = true;
            *self.live_wrap.borrow_mut() = None;
        }
        self.assistant_buffer.push_str(delta);
    }

    pub fn push_thinking(&mut self, text: impl Into<String>) {
        self.flush_assistant();
        self.push_item(FlowItem::Thinking(text.into()));
    }

    pub fn toggle_thinking(&mut self) {
        self.thinking_collapsed = !self.thinking_collapsed;
        // 折叠态影响所有 thinking 条目的渲染。
        self.row_cache.borrow_mut().invalidate_all();
    }

    pub fn thinking_collapsed(&self) -> bool {
        self.thinking_collapsed
    }

    /// 落定当前 assistant 段落（若有）。
    pub fn flush_assistant(&mut self) {
        if self.assistant_active {
            let text = std::mem::take(&mut self.assistant_buffer);
            self.push_item(FlowItem::Text {
                style: NoteStyle::Info,
                text,
            });
            self.assistant_active = false;
            *self.live_wrap.borrow_mut() = None;
        }
    }

    /// 工具开始：建立运行中记录；未知重复 id 保持原记录（幂等）。
    pub fn tool_start(&mut self, call_id: &str, name: &str, args: &serde_json::Value) {
        self.flush_assistant();
        if self.tool_item_index(call_id).is_some() {
            return;
        }
        let serialized = serde_json::to_string(args).unwrap_or_default();
        self.push_item(FlowItem::Tool(ToolItem {
            call_id: call_id.to_string(),
            name: name.to_string(),
            args_head: truncate_label(&serialized, 120),
            state: ToolState::Running {
                last_output: String::new(),
            },
        }));
    }

    /// 工具增量：仅刷新对应运行中记录的预览，不新增条目。
    pub fn tool_update(&mut self, call_id: &str, partial_output: &str) {
        if let Some(index) = self.tool_item_index(call_id) {
            self.row_cache.borrow_mut().invalidate(index);
            let item = &mut self.items[index];
            if let FlowItem::Tool(tool) = item
                && let ToolState::Running { last_output } = &mut tool.state
            {
                *last_output = truncate_label(partial_output, 200);
            }
        }
    }

    /// 工具结束：就地定型为稳定记录；首个终态生效，重复终态保持首见结果。
    pub fn tool_end(&mut self, call_id: &str, result: &str, is_error: bool) {
        if let Some(index) = self.tool_item_index(call_id) {
            self.row_cache.borrow_mut().invalidate(index);
            let item = &mut self.items[index];
            if let FlowItem::Tool(tool) = item
                && matches!(tool.state, ToolState::Running { .. })
            {
                tool.state = ToolState::Done {
                    output: result.to_string(),
                    is_error,
                    display: ToolDisplay::Truncated,
                    completed_at: std::time::Instant::now(),
                };
                self.last_completed_at = Some(std::time::Instant::now());
            }
        }
    }

    /// 条目终态：在未收到 ToolExecutionEnd 时把运行中工具定型为稳定记录。
    /// 取消/异常中断时 `ItemCompleted`/`ItemFailed` 是唯一收尾信号，工具块
    /// 不能停留在 Running；输出回退到最后一次增量预览。
    pub fn tool_terminal(&mut self, call_id: &str, is_error: bool) {
        if let Some(index) = self.tool_item_index(call_id) {
            self.row_cache.borrow_mut().invalidate(index);
            let item = &mut self.items[index];
            let FlowItem::Tool(tool) = item else {
                return;
            };
            let ToolState::Running { last_output } = &mut tool.state else {
                return;
            };
            let output = std::mem::take(last_output);
            tool.state = ToolState::Done {
                output,
                is_error,
                display: ToolDisplay::Truncated,
                completed_at: std::time::Instant::now(),
            };
            self.last_completed_at = Some(std::time::Instant::now());
        }
    }

    /// 切换最近一个已完成工具块的展开态（折叠→截断→完整循环）。
    /// 运行中或没有已完成工具时为 no-op。
    pub fn toggle_latest_tool_expansion(&mut self) {
        for (index, item) in self.items.iter_mut().enumerate().rev() {
            if let FlowItem::Tool(tool) = item
                && let ToolState::Done { display, .. } = &mut tool.state
            {
                *display = display.next();
                self.row_cache.borrow_mut().invalidate(index);
                return;
            }
        }
    }

    fn tool_item_index(&self, call_id: &str) -> Option<usize> {
        self.tool_index.get(call_id).copied()
    }

    /// 该 item_id 是否为工具条目（runtime 中工具条目的 item id 即
    /// tool call id）。事件投影据此区分工具相关事件与 assistant 文本。
    pub fn is_tool_item(&self, call_id: &str) -> bool {
        self.tool_item_index(call_id).is_some()
    }

    /// 进行中 assistant 段落的可视行数：未落定内容随帧实时可见。
    pub fn live_row_count(&self, width: u16) -> usize {
        if !self.assistant_active {
            return 0;
        }
        self.live_lines(width.max(1)).len().max(1)
    }

    /// 进行中 assistant 段落的全部可视行：未落定内容随帧实时可见。
    pub fn live_rows(&self, width: u16) -> Vec<Line<'static>> {
        if !self.assistant_active {
            return Vec::new();
        }
        self.live_lines(width.max(1))
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), NoteStyle::Info.style())))
            .collect()
    }

    /// 完成闪烁窗口内：定型工具的完成态样式仍随帧变化，事件循环不得跳帧。
    pub fn completion_flash_active(&self) -> bool {
        self.last_completed_at
            .is_some_and(|at| at.elapsed() < TOOL_COMPLETION_FLASH)
    }

    /// 折行一次、帧内复用：计数与逐行渲染共享同一份折行结果。段落内缓冲
    /// 只追加不修改，宽度不变且长度未收缩时只重包新增后缀（末行残行续算），
    /// 单次增量正比于新增文本而非整个缓冲。
    fn live_lines(&self, width: u16) -> std::cell::Ref<'_, Vec<String>> {
        {
            let mut slot = self.live_wrap.borrow_mut();
            let buffered = self.assistant_buffer.len();
            let can_append = slot
                .as_ref()
                .is_some_and(|cached| cached.width == width && buffered >= cached.consumed);
            if can_append {
                if let Some(cached) = slot.as_mut()
                    && buffered > cached.consumed
                {
                    let suffix = &self.assistant_buffer[cached.last_start..];
                    let mut fresh = wrapped_lines(suffix, width as usize);
                    cached.lines.pop();
                    cached.lines.append(&mut fresh);
                    let last_len = cached.lines.last().map_or(0, String::len);
                    cached.last_start = buffered - last_len;
                    cached.consumed = buffered;
                }
            } else {
                let lines = wrapped_lines(&self.assistant_buffer, width as usize);
                let last_len = lines.last().map_or(0, String::len);
                *slot = Some(LiveWrap {
                    width,
                    lines,
                    consumed: buffered,
                    last_start: buffered - last_len,
                });
            }
        }
        static EMPTY: Vec<String> = Vec::new();
        std::cell::Ref::map(self.live_wrap.borrow(), |slot| {
            slot.as_ref().map(|cached| &cached.lines).unwrap_or(&EMPTY)
        })
    }

    /// 在给定宽度下每个条目占用的可视行数。静态条目读缓存，随帧变化
    /// 的条目（运行中工具、完成闪烁）实时物化。
    pub fn row_counts(&self, width: u16) -> Vec<usize> {
        let width = width.max(1) as usize;
        let mut cache = self.row_cache.borrow_mut();
        cache.ensure_width(width as u16, self.thinking_collapsed);
        self.items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                if item_is_frame_variant(item) {
                    item_rows(item, width, self.thinking_collapsed, ' ').len()
                } else {
                    let slot = &mut cache.rows[index];
                    let rows = slot.get_or_insert_with(|| {
                        item_rows(item, width, self.thinking_collapsed, ' ')
                    });
                    rows.len()
                }
            })
            .collect()
    }

    /// 物化某条目的全部可视行（单次计算）：调用方做窗口切片。随帧变化的
    /// 条目（运行中工具、完成闪烁）实时物化，静态条目读缓存。
    /// `spinner` 为运行中工具的状态字符，由调用方按节拍提供。
    pub fn render_item_rows(
        &self,
        item_index: usize,
        width: u16,
        spinner: char,
    ) -> Vec<Line<'static>> {
        let width = width.max(1) as usize;
        // 末尾下标是进行中的 assistant 伪条目：与定稿条目同走这一条渲染路径，
        // 调用方不必为它另开一份窗口切片。
        if item_index == self.items.len() {
            return self.live_rows(width as u16);
        }
        let Some(item) = self.items.get(item_index) else {
            return Vec::new();
        };
        if item_is_frame_variant(item) {
            return item_rows(item, width, self.thinking_collapsed, spinner);
        }
        let mut cache = self.row_cache.borrow_mut();
        cache.ensure_width(width as u16, self.thinking_collapsed);
        match cache.rows.get_mut(item_index) {
            Some(slot) => slot
                .get_or_insert_with(|| item_rows(item, width, self.thinking_collapsed, ' '))
                .clone(),
            None => Vec::new(),
        }
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
            let mut rows: Vec<Line<'static>> =
                wrapped_lines(&tool.header(), width.saturating_sub(2))
                    .into_iter()
                    .map(|line| Line::from(Span::styled(format!("{marker} {line}"), style)))
                    .collect();
            match &tool.state {
                ToolState::Running { last_output } => {
                    if !last_output.trim().is_empty() {
                        rows.extend(bounded_preview(last_output).into_iter().map(|line| {
                            Line::from(Span::styled(format!("│ {line}"), NoteStyle::Dim.style()))
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
                    let total_nonempty = output.lines().filter(|l| !l.trim().is_empty()).count();
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
                    rows.extend(
                        visible.iter().copied().map(|line| {
                            Line::from(Span::styled(format!("│ {line}"), result_style))
                        }),
                    );
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

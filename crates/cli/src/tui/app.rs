//! TUI 应用状态：输入路由、滚动收敛、footer 合同与整帧渲染。
//!
//! [`TuiApp`] 是可独立测试的纯状态对象（渲染走 ratatui `TestBackend`）：
//! 键盘事件经 [`TuiApp::handle_key`] 变更状态或产生动作；turn 事件经
//! [`TuiApp::on_turn_event`] 投影进会话流。业务队列只有一份——followUp
//! 与设置意图全部存放在 `Conversation`，这里只提交输入并展示计数。

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use singularity_runtime::events::TurnEvent;
use singularity_runtime::objects::TurnStatus;
use singularity_runtime::{Conversation, SettingsPatch};

use super::editor::Editor;
use super::scroll::ScrollState;
use super::transcript::{NoteStyle, Transcript};

const SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];
const MAX_EDITOR_ROWS_CAP: u16 = 10;
/// 鼠标滚轮一格对应的三行滚动。
pub(crate) const WHEEL_ROWS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Running,
    Interrupting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputMode {
    Steer,
    FollowUp,
}

impl InputMode {
    fn toggled(self) -> Self {
        match self {
            Self::Steer => Self::FollowUp,
            Self::FollowUp => Self::Steer,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

/// 当前正在等待的对象：驱动状态行的具名活动提示。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum WaitingTarget {
    #[default]
    None,
    /// 等待模型响应或流式输出。
    Model,
    /// 等待指定工具执行完成。
    Tool(String),
    /// Agent 已停止，等待终态落盘与事件收口。
    TerminalConvergence,
}

impl WaitingTarget {
    fn label(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Model => Some("model".to_string()),
            Self::Tool(name) => Some(format!("tool {name}")),
            Self::TerminalConvergence => Some("terminal convergence".to_string()),
        }
    }
}

/// 设置面板的临时编辑状态。
pub(crate) struct SettingsMenu {
    field: usize,
    provider: String,
    model: String,
    reasoning: String,
    error: Option<String>,
}

impl SettingsMenu {
    pub fn open(current_model: Option<&str>) -> Self {
        let parts = singularity_model::split_model_selector(current_model.unwrap_or_default());
        Self {
            field: 0,
            provider: parts.provider.unwrap_or("openai_compatible").to_string(),
            model: parts.model.unwrap_or_default().to_string(),
            reasoning: parts.effort.unwrap_or_default().to_string(),
            error: None,
        }
    }

    fn fields(&self) -> [&String; 3] {
        [&self.provider, &self.model, &self.reasoning]
    }

    fn current_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.provider,
            1 => &mut self.model,
            _ => &mut self.reasoning,
        }
    }

    fn patch(&self) -> SettingsPatch {
        let optional = |value: &String| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        SettingsPatch {
            provider: optional(&self.provider),
            model: optional(&self.model),
            reasoning: optional(&self.reasoning),
        }
    }
}

/// 键盘处理结果：继续或提交一轮输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Continue,
    Submit(String),
}

/// 交互式会话的应用状态。
pub(crate) struct TuiApp {
    conversation: Arc<Conversation>,
    transcript: Transcript,
    scroll: ScrollState,
    editor: Editor,
    input_mode: InputMode,
    phase: Phase,
    waiting: WaitingTarget,
    settings: Option<SettingsMenu>,
    thread_id: String,
    quit_hint: bool,
    spinner_frame: usize,
    /// 当前等待对象开始等待的时刻（状态行相位计时）。
    waiting_since: Option<std::time::Instant>,
    /// 最近一帧会话流宽度、总行数与视口高（键位滚动与增长检测依赖）。
    last_flow_width: Option<u16>,
    last_total_rows: usize,
    last_viewport_rows: usize,
}

impl TuiApp {
    pub fn new(conversation: Arc<Conversation>) -> Self {
        let thread_id = conversation
            .thread()
            .map(|thread| thread.thread_id)
            .unwrap_or_default();
        Self {
            conversation,
            transcript: Transcript::new(),
            scroll: ScrollState::default(),
            editor: Editor::new(),
            input_mode: InputMode::Steer,
            phase: Phase::Idle,
            waiting: WaitingTarget::None,
            settings: None,
            thread_id,
            quit_hint: false,
            spinner_frame: 0,
            waiting_since: None,
            last_flow_width: None,
            last_total_rows: 0,
            last_viewport_rows: 5,
        }
    }

    // -- 状态推进 ------------------------------------------------------------

    fn set_waiting(&mut self, target: WaitingTarget) {
        if self.waiting != target {
            self.waiting = target;
            self.waiting_since = Some(std::time::Instant::now());
        }
    }

    pub fn on_turn_event(&mut self, event: &TurnEvent) {
        match event {
            TurnEvent::ThreadStarted { thread } => {
                self.transcript.push_note(
                    format!(
                        "thread {}",
                        &thread.thread_id[..8.min(thread.thread_id.len())]
                    ),
                    NoteStyle::Dim,
                );
            }
            TurnEvent::TurnStarted { turn } => {
                self.transcript.push_note(
                    format!("── turn {} ──", &turn.turn_id[..8.min(turn.turn_id.len())]),
                    NoteStyle::Dim,
                );
                self.set_waiting(WaitingTarget::Model);
            }
            TurnEvent::AssistantDelta { delta, item_id, .. } => {
                if Self::is_assistant(item_id) {
                    self.transcript.assistant_delta(delta);
                }
                self.set_waiting(WaitingTarget::Model);
            }
            TurnEvent::ProviderAttempt { status, .. } => {
                if status == "started" {
                    self.set_waiting(WaitingTarget::Model);
                }
            }
            // 聚合遥测不改变等待对象，也不进入会话流。
            TurnEvent::ProviderAttemptSummary { .. } => {}
            TurnEvent::ItemStarted { .. } => {}
            TurnEvent::ItemCompleted { item_id, .. } | TurnEvent::ItemFailed { item_id, .. } => {
                if !Self::is_assistant(item_id) {
                    self.set_waiting(WaitingTarget::Model);
                }
            }
            TurnEvent::ToolExecutionStart {
                tool_name,
                tool_call_id,
                args,
                ..
            } => {
                self.transcript.flush_assistant();
                self.transcript.tool_start(tool_call_id, tool_name, args);
                self.set_waiting(WaitingTarget::Tool(tool_name.clone()));
            }
            TurnEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                self.transcript.tool_update(tool_call_id, partial_result);
            }
            TurnEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                self.transcript.tool_end(tool_call_id, result, *is_error);
                self.set_waiting(WaitingTarget::Model);
            }
            TurnEvent::Diagnostic {
                severity,
                code,
                message,
                ..
            } => {
                let style = match severity.as_str() {
                    "error" => NoteStyle::Error,
                    "warning" => NoteStyle::Warning,
                    _ => NoteStyle::Dim,
                };
                self.transcript
                    .push_note(format!("⚠ [{severity}] {code}: {message}"), style);
            }
            TurnEvent::TurnCompleted { turn } => {
                self.transcript.push_note(
                    format!("✔ completed ({})", describe_usage(turn)),
                    NoteStyle::Dim,
                );
                self.set_waiting(WaitingTarget::TerminalConvergence);
            }
            TurnEvent::TurnFailed { error, .. } => {
                self.transcript.push_note(
                    format!("✖ failed [{}]: {}", error.cause, error.message),
                    NoteStyle::Error,
                );
                self.set_waiting(WaitingTarget::TerminalConvergence);
            }
        }
    }

    fn is_assistant(item_id: &str) -> bool {
        item_id.ends_with("_assistant")
    }

    /// 整个 run_turn 调用结束（含其后续队列执行完毕）。
    pub fn on_chain_finished(&mut self, result: &Result<TurnStatus, String>) {
        self.phase = Phase::Idle;
        self.set_waiting(WaitingTarget::None);
        crate::signal::reset();
        match result {
            Ok(TurnStatus::Interrupted) => {
                self.transcript
                    .push_note("turn interrupted", NoteStyle::Warning);
            }
            Ok(_) => {}
            Err(message) => {
                self.transcript
                    .push_note(format!("✖ {message}"), NoteStyle::Error);
            }
        }
    }

    pub fn note_interrupt_requested(&mut self) {
        self.phase = Phase::Interrupting;
        self.quit_hint = false;
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn mark_quit_hint(&mut self) {
        self.quit_hint = true;
    }

    /// 推进 spinner 节拍（由事件循环按固定间隔调用）。
    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    // -- 键盘路由 ------------------------------------------------------------

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Action::Continue;
        }

        if let Some(menu) = self.settings.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.settings = None;
                }
                KeyCode::Tab => menu.field = (menu.field + 1) % 3,
                KeyCode::Backspace => {
                    menu.current_mut().pop();
                }
                KeyCode::Enter => {
                    let patch = menu.patch();
                    match self.conversation.queue_settings(patch) {
                        Ok(true) => {
                            let queued_now = self.phase != Phase::Idle;
                            self.transcript.push_note(
                                if queued_now {
                                    "settings queued; effective from the next turn"
                                } else {
                                    "settings updated for this thread"
                                },
                                NoteStyle::Accent,
                            );
                            self.settings = None;
                        }
                        Ok(false) => menu.error = Some("nothing to change".into()),
                        Err(error) => menu.error = Some(error.to_string()),
                    }
                }
                KeyCode::Char(ch) => menu.current_mut().push(ch),
                _ => {}
            }
            return Action::Continue;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('t') if ctrl => self.input_mode = self.input_mode.toggled(),
            KeyCode::Char('s') if ctrl => {
                let current = self.conversation.thread().ok().and_then(|t| t.model);
                self.settings = Some(SettingsMenu::open(current.as_deref()));
            }
            KeyCode::Char('j') if ctrl => self.editor.insert_newline(),
            KeyCode::Char('o') if alt => {
                self.transcript.toggle_latest_tool_expansion();
            }
            KeyCode::Esc => {
                // 阶梯式退出，不留死端：浏览态先回底跟随；随后清空非空草稿；
                // 空输入时为 no-op（提示行不会承诺按键行为）。
                let (total, viewport) = self.flow_metrics();
                if !self.scroll.is_following() {
                    self.scroll.jump_to_bottom(total, viewport);
                } else if !self.editor.is_empty() {
                    self.editor.clear();
                }
            }
            KeyCode::Home if ctrl => self.scroll.jump_to_top(),
            KeyCode::End if ctrl => {
                let (total, viewport) = self.flow_metrics();
                self.scroll.jump_to_bottom(total, viewport);
            }
            KeyCode::PageUp => {
                let (total, viewport) = self.flow_metrics();
                let page = viewport.saturating_sub(2).max(1);
                self.scroll.scroll_up(page, total, viewport);
            }
            KeyCode::PageDown => {
                let (total, viewport) = self.flow_metrics();
                let page = viewport.saturating_sub(2).max(1);
                self.scroll.scroll_down(page, total, viewport);
            }
            KeyCode::Up if alt => {
                let (total, viewport) = self.flow_metrics();
                self.scroll.scroll_up(1, total, viewport);
            }
            KeyCode::Down if alt => {
                let (total, viewport) = self.flow_metrics();
                self.scroll.scroll_down(1, total, viewport);
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.editor.insert_newline();
            }
            KeyCode::Enter => return self.submit_input(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => self.editor.move_right(),
            KeyCode::Up => self.editor.move_up(),
            KeyCode::Down => self.editor.move_down(),
            KeyCode::Home => self.editor.move_home(),
            KeyCode::End => self.editor.move_end(),
            KeyCode::Char(ch) if !ctrl && !alt => self.editor.insert_char(ch),
            _ => {}
        }
        Action::Continue
    }

    /// 鼠标滚轮：向上脱离跟随，向下触底恢复。
    pub fn handle_wheel(&mut self, up: bool) {
        let (total, viewport) = self.flow_metrics();
        if up {
            self.scroll.scroll_up(WHEEL_ROWS, total, viewport);
        } else {
            self.scroll.scroll_down(WHEEL_ROWS, total, viewport);
        }
    }

    fn submit_input(&mut self) -> Action {
        let raw = self.editor.take();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return Action::Continue;
        }
        let action = match self.phase {
            Phase::Idle => {
                self.phase = Phase::Running;
                self.quit_hint = false;
                self.waiting = WaitingTarget::Model;
                crate::signal::reset();
                Action::Submit(text)
            }
            Phase::Running | Phase::Interrupting => {
                match self.input_mode {
                    InputMode::Steer => {
                        let accepted = self.conversation.steer(text.clone());
                        self.note_injection("steer", accepted, &text);
                    }
                    InputMode::FollowUp => {
                        let accepted = self.conversation.submit_follow_up(text.clone());
                        self.note_injection("followUp", accepted, &text);
                    }
                }
                Action::Continue
            }
        };
        // 发送输入后回到最新内容。
        let (total, viewport) = self.flow_metrics();
        self.scroll.jump_to_bottom(total, viewport);
        action
    }

    fn note_injection(&mut self, kind: &str, accepted: bool, text: &str) {
        let label = truncate_label(text, 40);
        self.transcript.push_note(
            if accepted {
                format!("↳ {kind}: {label}")
            } else {
                format!("↳ {kind} rejected (turn closed): {label}")
            },
            NoteStyle::Accent,
        );
    }

    // -- 渲染 ----------------------------------------------------------------

    fn flow_metrics(&self) -> (usize, usize) {
        let width = self.last_flow_width.unwrap_or(80);
        let total: usize = self.transcript.row_counts(width).iter().sum();
        (total, self.last_viewport_rows.max(1))
    }

    /// 整帧渲染。
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let inner_width = area.width.saturating_sub(2).max(1);

        let max_editor_rows = (area.height.saturating_sub(4) / 2)
            .clamp(3, MAX_EDITOR_ROWS_CAP)
            .max(1);
        let editor_rows = self.editor.display_height(inner_width, max_editor_rows) + 2;
        let flow_h = area
            .height
            .saturating_sub(editor_rows.saturating_add(2))
            .max(1);
        let [flow, editor_area, status_area, hint_area] = Layout::vertical([
            Constraint::Length(flow_h),
            Constraint::Length(editor_rows),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        // 滚动收敛：内容增长后按当前视口同步。
        self.last_flow_width = Some(flow.width);
        let counts = self.transcript.row_counts(flow.width);
        let total_rows: usize = counts.iter().sum();
        let viewport = flow.height as usize;
        let grown = total_rows.saturating_sub(self.last_total_rows);
        self.scroll.on_content_grow(grown, total_rows, viewport);
        self.last_total_rows = total_rows;
        self.last_viewport_rows = viewport;

        // 可视窗口物化：只渲染可见行。
        let top = if self.scroll.is_following() {
            total_rows.saturating_sub(viewport)
        } else {
            self.scroll.top_row()
        };
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
        if total_rows > 0 && viewport > 0 {
            let spinner = SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
            let spinner = if self.phase == Phase::Idle || self.waiting == WaitingTarget::None {
                ' '
            } else {
                spinner
            };
            let mut offset = 0usize;
            let mut emitted = 0usize;
            let mut first_overlap_done = false;
            for (item_index, rows) in counts.iter().enumerate() {
                if emitted >= viewport {
                    break;
                }
                let end = offset + rows;
                if end <= top {
                    offset = end;
                    continue;
                }
                let start_in_item = if first_overlap_done {
                    0
                } else {
                    top.saturating_sub(offset)
                };
                first_overlap_done = true;
                offset = end;
                for row_in_item in start_in_item..*rows {
                    if emitted >= viewport {
                        break;
                    }
                    if let Some(line) = self.transcript.render_item_row(
                        item_index,
                        row_in_item,
                        flow.width,
                        spinner,
                    ) {
                        lines.push(line);
                        emitted += 1;
                    }
                }
            }
        }
        while (lines.len() as u16) < flow.height {
            lines.push(Line::from(Span::raw(String::new())));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), flow);

        // 编辑器：高度随内容增长（钳制上限），光标始终可见。
        let editor_inner_w = editor_area.width.saturating_sub(2).max(1);
        let inner_h = editor_rows.saturating_sub(2) as usize;
        let (visual_row, visual_col) = self.editor.cursor_visual(editor_inner_w);
        let scroll_top = visual_row.saturating_sub(inner_h.saturating_sub(1));
        let mut editor_lines: Vec<Line<'static>> = Vec::new();
        for logical in self.editor.lines() {
            for piece in wrap_plain(logical, editor_inner_w as usize) {
                editor_lines.push(Line::from(Span::raw(piece)));
            }
        }
        frame.render_widget(
            Paragraph::new(editor_lines)
                .block(Block::default().borders(Borders::ALL).title("input"))
                .scroll((scroll_top as u16, 0)),
            editor_area,
        );

        // 状态行 + 提示行。
        let (status_spans, hint_spans) = self.footer_spans(total_rows, viewport);
        frame.render_widget(Paragraph::new(vec![Line::from(status_spans)]), status_area);
        frame.render_widget(Paragraph::new(vec![Line::from(hint_spans)]), hint_area);

        if let Some(menu) = &self.settings {
            self.render_settings(frame, menu);
        }

        // 光标定位到编辑器内。
        let cursor_y = editor_area
            .y
            .saturating_add(1 + visual_row.saturating_sub(scroll_top) as u16);
        let cursor_x = editor_area.x.saturating_add(1 + visual_col as u16);
        if cursor_y < editor_area.y.saturating_add(editor_area.height) {
            frame.set_cursor_position(ratatui::layout::Position::new(cursor_x, cursor_y));
        }
    }

    fn render_settings(&self, frame: &mut Frame<'_>, menu: &SettingsMenu) {
        let popup = centered_rect(frame.area(), 60, 9);
        frame.render_widget(Clear, popup);
        let names = ["provider", "model", "reasoning"];
        let values = menu.fields();
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let style = if index == menu.field {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            lines.push(Line::from(Span::styled(
                format!("{name:>9}: {}", values[index]),
                style,
            )));
        }
        if let Some(error) = &menu.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(Color::Red),
            )));
        }
        lines.push(Line::from(Span::styled(
            "Enter apply · Tab next field · Esc close",
            Style::new().fg(Color::DarkGray),
        )));
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("thread settings"),
            ),
            popup,
        );
    }

    /// footer 合同：状态行＝相位+spinner·具名等待对象·thread·模型；右侧＝
    /// 输入模式·队列数·浏览指示（含新增计数）。提示行按上下文给出关键操作。
    pub fn footer_spans(
        &self,
        total_rows: usize,
        viewport: usize,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let dim = Style::new().fg(Color::DarkGray);
        let accent = Style::new().fg(Color::Cyan);
        let warn = Style::new().fg(Color::Yellow);
        let magenta = Style::new().fg(Color::Magenta);

        let mut status = vec![];
        if self.phase != Phase::Idle {
            let spinner = SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
            let phase_word = match self.phase {
                Phase::Running => "running",
                Phase::Interrupting => "interrupting",
                Phase::Idle => unreachable!("guarded above"),
            };
            status.push(Span::styled(format!("{spinner} {phase_word}"), warn));
            if let Some(target) = self.waiting.label() {
                let elapsed = self
                    .waiting_since
                    .map(|since| since.elapsed().as_secs())
                    .unwrap_or(0);
                status.push(Span::styled(
                    format!(" · waiting: {target} {elapsed}s"),
                    warn,
                ));
            }
        } else {
            status.push(Span::styled("idle", dim));
        }
        status.push(Span::styled(
            format!(" · thread {} · ", short_id(&self.thread_id)),
            dim,
        ));
        match self.conversation.thread().ok().and_then(|t| t.model) {
            Some(model) => status.push(Span::styled(format!("{model} · "), dim)),
            None => status.push(Span::styled("model unset · ", warn)),
        }
        status.push(Span::styled(
            format!("[{}]", self.input_mode.label()),
            accent,
        ));
        let queue = self.conversation.pending_follow_ups().len();
        if queue > 0 {
            status.push(Span::styled(format!(" queue:{queue}"), warn));
        }
        if !self.scroll.is_following() {
            let at_bottom = self.scroll.top_row() >= total_rows.saturating_sub(viewport);
            if self.scroll.pending_below() > 0 && !at_bottom {
                status.push(Span::styled(
                    format!(" ↓{} new", self.scroll.pending_below()),
                    warn,
                ));
            }
            status.push(Span::styled(" · viewing history", magenta));
        }

        let hint_text = if self.settings.is_some() {
            "Esc close · Tab field · Enter apply"
        } else if self.quit_hint {
            "press Ctrl+C again to quit"
        } else {
            match self.phase {
                Phase::Idle => {
                    "Enter send · Shift+Enter newline · Ctrl+S settings · PgUp/PgDn scroll · Ctrl+End latest"
                }
                Phase::Running | Phase::Interrupting => {
                    "Enter steer/followUp · Ctrl+T switch · Alt+O expand tool · Ctrl+C interrupt (twice force)"
                }
            }
        };
        let hint_style = if self.quit_hint {
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let hint = vec![Span::styled(hint_text, hint_style)];
        (status, hint)
    }

    // -- 测试辅助 -----------------------------------------------------------

    #[cfg(test)]
    fn push_test_note(&mut self, text: &str) {
        self.transcript.push_note(text.to_string(), NoteStyle::Info);
    }

    #[cfg(test)]
    fn scroll_snapshot(&self) -> (bool, usize, usize) {
        (
            self.scroll.is_following(),
            self.scroll.top_row(),
            self.scroll.pending_below(),
        )
    }

    #[cfg(test)]
    fn force_phase(&mut self, phase: Phase) {
        self.phase = phase;
    }

    #[cfg(test)]
    fn set_mode(&mut self, mode: InputMode) {
        self.input_mode = mode;
    }

    #[cfg(test)]
    fn editor_text(&self) -> String {
        self.editor.text()
    }
}

fn describe_usage(turn: &singularity_runtime::objects::Turn) -> String {
    match &turn.usage {
        Some(usage) if usage.usage_present => format!(
            "{} in / {} out tokens",
            usage.input_tokens, usage.output_tokens
        ),
        _ => "usage unavailable".to_string(),
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn truncate_label(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{cut}…")
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for logical in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in logical.chars() {
            let w = crate::tui::char_display_width(ch);
            if current_width + w > width && !current.is_empty() {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += w;
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let width = area.width.saturating_mul(percent_x) / 100;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use singularity_runtime::TurnRunner;
    use std::sync::mpsc;

    // -- 夹具 ----------------------------------------------------------------

    struct NeverCalledProvider;

    impl singularity_model::Provider for NeverCalledProvider {
        fn protocol_contract(&self) -> singularity_model::ProviderProtocolContract {
            singularity_model::ProviderProtocolContract::default()
        }
        fn complete(
            &self,
            _request: &singularity_model::ModelTurnRequest,
            _cancellation: &singularity_core::CancellationToken,
        ) -> Result<singularity_model::ModelTurnResponse, singularity_model::ProviderError>
        {
            panic!("provider must not be called in this test")
        }
    }

    /// 在 provider 内部挂起直到外部放行（用于制造活动 turn 窗口）。
    struct GatedOnceProvider {
        release: std::sync::Mutex<mpsc::Receiver<()>>,
    }

    impl singularity_model::Provider for GatedOnceProvider {
        fn protocol_contract(&self) -> singularity_model::ProviderProtocolContract {
            singularity_model::ProviderProtocolContract::default()
        }
        fn complete(
            &self,
            request: &singularity_model::ModelTurnRequest,
            _cancellation: &singularity_core::CancellationToken,
        ) -> Result<singularity_model::ModelTurnResponse, singularity_model::ProviderError>
        {
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(3));
            Ok(singularity_model::ModelTurnResponse::completed(
                request.request_id.clone(),
                "r",
                "ok",
            ))
        }
    }

    fn test_home() -> (&'static tempfile::TempDir, std::path::PathBuf) {
        let dir = Box::leak(Box::new(tempfile::TempDir::new().expect("temp home")));
        let sessions = dir.path().join("sessions");
        (dir, sessions)
    }

    fn test_conversation(
        sessions: &std::path::Path,
        provider: std::sync::Arc<dyn singularity_model::Provider + Send + Sync>,
    ) -> Arc<Conversation> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();
        std::mem::forget(runtime);
        let snapshot = singularity_model::ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL" => Some("base-model".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:9/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key-placeholder".to_string()),
                _ => None,
            },
            handle,
        );
        let runner = Arc::new(
            TurnRunner::new(sessions.to_path_buf(), snapshot).with_provider_override(provider),
        );
        let thread = singularity_runtime::store::create_thread(
            sessions,
            std::env::current_dir().unwrap().to_str().unwrap(),
            Some("openai_compatible/base-model".to_string()),
        )
        .expect("create thread");
        Conversation::new(runner, thread)
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn draw_at(app: &mut TuiApp, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| app.draw(frame))
            .expect("draw succeeds");
        terminal
    }

    /// 收集指定行范围内的可见文本。
    fn row_text(terminal: &Terminal<TestBackend>, y: u16, width: u16) -> String {
        let buffer = terminal.backend().buffer();
        let mut line = String::new();
        for x in 0..width {
            line.push_str(buffer[(x, y)].symbol());
        }
        line.trim_end().to_string()
    }

    // -- 滚动与渲染 ----------------------------------------------------------

    #[test]
    fn following_stays_pinned_to_latest_output() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        for index in 1..=40 {
            app.push_test_note(&format!("note-{index}"));
        }
        let terminal = draw_at(&mut app, 80, 24);
        let (follow, _top, pending) = app.scroll_snapshot();
        assert!(follow, "fresh content keeps following");
        assert_eq!(pending, 0);
        // 视口底行显示最新条目。
        let bottom = row_text(&terminal, 18, 80);
        assert!(
            bottom.contains("note-40"),
            "latest output must be visible, got: {bottom:?}"
        );
    }

    #[test]
    fn browsing_holds_position_and_counts_new_content_until_recalled() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        for index in 1..=40 {
            app.push_test_note(&format!("note-{index}"));
        }
        draw_at(&mut app, 80, 24); // 建立真实视口度量。
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        let (follow, top, _) = app.scroll_snapshot();
        assert!(!follow, "PageUp enters browsing state");
        let held_top = top;

        // 新内容到达：浏览位置不动，底部新增被计数。
        for index in 41..=47 {
            app.push_test_note(&format!("note-{index}"));
        }
        draw_at(&mut app, 80, 24);
        let (follow, top_after, pending) = app.scroll_snapshot();
        assert!(!follow);
        assert_eq!(top_after, held_top, "browsing position must not jump");
        assert_eq!(pending, 7);

        // 快捷键回底：跟随恢复、计数清零。
        app.handle_key(key(KeyCode::End, KeyModifiers::CONTROL));
        let (follow, _, pending) = app.scroll_snapshot();
        assert!(follow);
        assert_eq!(pending, 0);
        let terminal = draw_at(&mut app, 80, 24);
        assert!(row_text(&terminal, 18, 80).contains("note-47"));
    }

    #[test]
    fn resize_keeps_browsing_anchor_without_content_loss() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        for index in 1..=60 {
            app.push_test_note(&format!("note-{index}"));
        }
        draw_at(&mut app, 80, 24);
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        let (_, top_before, _) = app.scroll_snapshot();

        draw_at(&mut app, 100, 30);
        let (follow, top_after, _) = app.scroll_snapshot();
        assert!(!follow, "resize must not force reattach");
        assert!(
            top_after <= top_before,
            "anchor must stay within clamped bounds"
        );

        // 缩小窗口后跟随态仍钉底。
        app.handle_key(key(KeyCode::End, KeyModifiers::CONTROL));
        draw_at(&mut app, 70, 20);
        let (follow, _top, _) = app.scroll_snapshot();
        assert!(follow);
    }

    // -- 设置模态 ------------------------------------------------------------

    #[test]
    fn settings_modal_preserves_scroll_and_composes_patch() {
        let menu = SettingsMenu::open(Some("prov/m#high"));
        let patch = menu.patch();
        assert_eq!(patch.provider.as_deref(), Some("prov"));
        assert_eq!(patch.model.as_deref(), Some("m"));
        assert_eq!(patch.reasoning.as_deref(), Some("high"));

        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        for index in 1..=30 {
            app.push_test_note(&format!("note-{index}"));
        }
        draw_at(&mut app, 80, 24);
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        let snapshot_before = app.scroll_snapshot();

        app.handle_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        draw_at(&mut app, 80, 24);
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.scroll_snapshot(), snapshot_before);
        assert!(
            app.editor_text().is_empty(),
            "modal never steals the editor buffer"
        );
    }

    #[test]
    fn esc_staircase_returns_to_follow_then_clears_draft_without_dead_end() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        for index in 1..=40 {
            app.push_test_note(&format!("note-{index}"));
        }
        draw_at(&mut app, 80, 24);
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        let (follow, top, _) = app.scroll_snapshot();
        assert!(!follow);
        assert!(top > 0);

        // 第一级：浏览态 Esc → 回底跟随。
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        let (follow, _, pending) = app.scroll_snapshot();
        assert!(follow, "browsing Esc returns to follow");
        assert_eq!(pending, 0);

        // 第二级：非空草稿 Esc → 清空（pi/Codex 习惯）。
        for ch in "draft".chars() {
            app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(app.editor_text(), "draft");
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.editor_text().is_empty(), "Esc clears the draft");

        // 第三级：空输入 + 跟随态 Esc → no-op，状态保持。
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        let (follow, _, _) = app.scroll_snapshot();
        assert!(follow);
        assert!(app.editor_text().is_empty());
    }

    // -- footer 合同 ---------------------------------------------------------

    #[test]
    fn footer_shows_phase_waiting_mode_and_history_indicator() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        app.force_phase(Phase::Running);
        app.on_turn_event(&TurnEvent::TurnStarted {
            turn: singularity_runtime::objects::Turn {
                turn_id: "abcd1234-rest".to_string(),
                thread_id: "thread-1".to_string(),
                status: TurnStatus::Running,
                usage: None,
            },
        });
        let (status, hint) = app.footer_spans(100, 20);
        let status_text: String = status.iter().map(|span| span.content.clone()).collect();
        assert!(status_text.contains("running"), "{status_text}");
        assert!(status_text.contains("waiting: model"), "{status_text}");
        assert!(status_text.contains("[steer]"), "{status_text}");
        assert!(
            status_text.contains("openai_compatible/base-model"),
            "{status_text}"
        );
        let hint_text: String = hint.iter().map(|span| span.content.clone()).collect();
        assert!(hint_text.contains("Ctrl+T"), "{hint_text}");

        app.handle_wheel(true);
        let (status, _) = app.footer_spans(100, 20);
        let status_text: String = status.iter().map(|span| span.content.clone()).collect();
        assert!(
            status_text.contains("viewing history"),
            "detached state must be announced"
        );

        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let (status, _) = app.footer_spans(100, 20);
        let status_text: String = status.iter().map(|span| span.content.clone()).collect();
        assert!(status_text.contains("[followUp]"), "{status_text}");

        app.mark_quit_hint();
        let (_, hint) = app.footer_spans(100, 20);
        let hint_text: String = hint.iter().map(|span| span.content.clone()).collect();
        assert!(hint_text.contains("Ctrl+C again"), "{hint_text}");
    }

    // -- 输入路由：followUp 单队列 -------------------------------------------

    #[test]
    fn follow_up_submission_enqueues_exactly_once_in_conversation() {
        let (_home, sessions) = test_home();
        let (release_tx, release_rx) = mpsc::channel();
        let conversation = test_conversation(
            &sessions,
            Arc::new(GatedOnceProvider {
                release: std::sync::Mutex::new(release_rx),
            }),
        );
        let mut app = TuiApp::new(Arc::clone(&conversation));

        let (tx, rx) = mpsc::channel::<crate::tui::UiEvent>();
        // 生产路径：编辑器输入 + Enter 提交初始轮（事件循环随后 spawn）。
        for ch in "initial goal".chars() {
            app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Submit(goal) => crate::tui::spawn_turn(&conversation, goal, tx.clone()),
            other => panic!("expected Submit action, got {other:?}"),
        }
        for _ in 0..400 {
            if conversation.has_active_turn() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(conversation.has_active_turn(), "gated turn must be active");
        assert_eq!(app.phase(), Phase::Running);

        // 与生产路径一致：编辑器输入 + Enter，以 followUp 模式提交。
        app.set_mode(InputMode::FollowUp);
        for ch in "second".chars() {
            app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(app.editor_text(), "second");
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            app.editor_text().is_empty(),
            "submitted input leaves the editor"
        );
        assert_eq!(
            conversation.pending_follow_ups(),
            vec!["second"],
            "the queue lives in Conversation and accepts the entry once"
        );

        release_tx.send(()).expect("release");
        // 等待链条结束（初始轮 + followUp 轮），保证夹具线程干净退出。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(16)) {
                Ok(crate::tui::UiEvent::ChainFinished(_)) => break,
                Ok(_) => {}
                Err(_) => panic!("chain should finish"),
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for chain finish");
            }
        }
        assert!(conversation.pending_follow_ups().is_empty());
    }

    #[test]
    fn idle_enter_submits_a_new_turn_action() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        for ch in "do it".chars() {
            app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Submit(goal) => assert_eq!(goal, "do it"),
            other => panic!("expected Submit action, got {other:?}"),
        }
        assert_eq!(app.phase(), Phase::Running);
    }
}

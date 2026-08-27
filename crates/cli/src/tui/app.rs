//! TUI 应用状态：输入路由、滚动收敛、footer 合同与整帧渲染。
//!
//! [`TuiApp`] 是可独立测试的纯状态对象（渲染走 ratatui `TestBackend`）。
//! 设置/恢复会话模态在 `modals`，会话动作在 `session_actions`，鼠标路由在
//! `mouse`，footer 合同在 `view`；跨模块共享的字段以 `pub(super)` 暴露。

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use singularity_runtime::Conversation;
use singularity_runtime::events::{AgentDiagnosticSeverity, ProviderAttemptStatus, TurnEvent};
use singularity_runtime::objects::TurnStatus;
use unicode_width::UnicodeWidthStr;

use super::commands::Action;
use super::editor::Editor;
use super::modals::{ResumeMenu, SettingsMenu};
use super::mouse::{ClickTarget, WheelNormalizer};
use super::scroll::ScrollState;
use super::transcript::{NoteStyle, Transcript};
use super::view::{describe_usage, truncate_label};
use super::wrapped_lines;

pub(super) const SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];
const MAX_EDITOR_ROWS_CAP: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Running,
    Interrupting,
}

/// 当前正在等待的对象：驱动状态行的具名活动提示。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum WaitingTarget {
    #[default]
    None,
    /// 等待模型响应或流式输出。
    Model,
    /// Provider 已开始本次生成，尚未收到可见回答文本。
    Thinking,
    /// 等待指定工具执行完成。
    Tool(String),
    /// Agent 已停止，等待终态落盘与事件收口。
    TerminalConvergence,
}

impl WaitingTarget {
    pub(super) fn label(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Model => Some("model".to_string()),
            Self::Thinking => Some("thinking".to_string()),
            Self::Tool(name) => Some(format!("tool {name}")),
            Self::TerminalConvergence => Some("terminal convergence".to_string()),
        }
    }
}

/// 交互式会话的应用状态。字段以 `pub(super)` 暴露给同模块树中的
/// `modals`/`session_actions`/`mouse`/`view`（同 `tui` 模块内部实现细节）。
pub(crate) struct TuiApp {
    pub(super) conversation: Arc<Conversation>,
    pub(super) transcript: Transcript,
    pub(super) scroll: ScrollState,
    pub(super) editor: Editor,
    pub(super) phase: Phase,
    pub(super) waiting: WaitingTarget,
    pub(super) settings: Option<SettingsMenu>,
    pub(super) resume: Option<ResumeMenu>,
    pub(super) thread_id: String,
    /// 二次确认退出已生效：下一次 Ctrl+C 正常退出（exit 0）。
    /// 复位规则：任何非 Ctrl+C 按键、提交输入或 turn 链结束都会清除；
    /// 按下期间提示行持续显示再次确认文案。
    pub(super) quit_armed: bool,
    pub(super) spinner_frame: usize,
    /// 当前等待对象开始等待的时刻（状态行相位计时）。
    pub(super) waiting_since: Option<std::time::Instant>,
    pub(super) turn_started_at: Option<std::time::Instant>,
    /// 状态行展示用累计 token 数（最后完成 turn 的聚合或 resume 时的摘要）：
    /// 仅存在已上报 usage 时展示，故为 Option。
    pub(super) session_tokens: Option<u64>,
    /// 最近一帧会话流宽度、总行数与视口高（键位滚动与增长检测依赖）。
    pub(super) last_flow_width: Option<u16>,
    pub(super) last_total_rows: usize,
    pub(super) last_viewport_rows: usize,
    /// 编辑器最近一帧的可视滚动顶行（点击定位换算依赖）。
    pub(super) last_editor_scroll_top: usize,
    /// 帧缓存点击矩形表：本帧渲染时登记 `(Rect, ClickTarget)` 对，鼠标
    /// 事件对缓存做包含测试（取代对状态行文本的反查）。
    pub(super) click_targets: Vec<(Rect, ClickTarget)>,
    /// 滚轮归一化状态（滚轮/触控板加速）。
    pub(super) wheel: WheelNormalizer,
    /// /compact 后台压缩进行中；Esc 通过 `compact_cancel` 中止本次压缩。
    pub(super) compacting: bool,
    pub(super) compact_cancel: Option<singularity_core::CancellationToken>,
    /// /compact 排队中：活动 turn 结束后自动启动压缩。
    pub(super) compact_queued: bool,
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
            phase: Phase::Idle,
            waiting: WaitingTarget::None,
            settings: None,
            resume: None,
            thread_id,
            quit_armed: false,
            spinner_frame: 0,
            waiting_since: None,
            turn_started_at: None,
            session_tokens: None,
            last_flow_width: None,
            last_total_rows: 0,
            last_viewport_rows: 5,
            last_editor_scroll_top: 0,
            click_targets: Vec::new(),
            wheel: WheelNormalizer::default(),
            compacting: false,
            compact_cancel: None,
            compact_queued: false,
        }
    }

    pub fn conversation_handle(&self) -> Arc<Conversation> {
        Arc::clone(&self.conversation)
    }

    // -- 状态推进 ------------------------------------------------------------

    pub(super) fn set_waiting(&mut self, target: WaitingTarget) {
        if self.waiting != target {
            self.waiting = target;
            self.waiting_since = Some(std::time::Instant::now());
        }
    }

    pub fn on_turn_event(&mut self, event: &TurnEvent) {
        match event {
            TurnEvent::TurnStarted { turn } => {
                self.transcript.push_note(
                    format!("── turn {} ──", &turn.turn_id[..8.min(turn.turn_id.len())]),
                    NoteStyle::Dim,
                );
                self.set_waiting(WaitingTarget::Model);
                self.turn_started_at = Some(std::time::Instant::now());
            }
            TurnEvent::AssistantDelta { delta, item_id, .. } => {
                if !self.transcript.is_tool_item(item_id) {
                    self.transcript.assistant_delta(delta);
                }
                self.set_waiting(WaitingTarget::Model);
            }
            TurnEvent::ProviderAttempt { status, .. } => {
                if *status == ProviderAttemptStatus::Started {
                    self.set_waiting(WaitingTarget::Thinking);
                }
            }
            // 条目开始不改变等待对象，也不进入会话流。
            TurnEvent::ItemStarted { .. } => {}
            // 条目终态是工具块的收尾信号：取消/异常中断时无 ToolExecutionEnd，
            // ItemFailed 是唯一定型入口，工具块不能停留在 Running。
            TurnEvent::ItemCompleted { item_id, .. } | TurnEvent::ItemFailed { item_id, .. } => {
                if self.transcript.is_tool_item(item_id) {
                    self.transcript.tool_terminal(
                        item_id,
                        matches!(event, TurnEvent::ItemFailed { .. }),
                    );
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
                let style = match severity {
                    AgentDiagnosticSeverity::Error => NoteStyle::Error,
                    AgentDiagnosticSeverity::Warning => NoteStyle::Warning,
                    AgentDiagnosticSeverity::Info => NoteStyle::Dim,
                };
                self.transcript
                    .push_note(format!("⚠ [{severity}] {code}: {message}"), style);
            }
            TurnEvent::TurnCompleted { turn } => {
                if turn.usage.as_ref().is_some_and(|usage| usage.usage_present) {
                    self.session_tokens = turn.usage.as_ref().map(|usage| usage.total_tokens);
                }
                if let Ok(blocks) = self.conversation.thinking_for_turn(&turn.turn_id) {
                    for block in blocks {
                        self.transcript.push_thinking(block);
                    }
                }
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
            // 待生效设置已在可信终态后应用，无需额外文案（提交点已提示过）。
            TurnEvent::ThreadSettingsApplied { .. } => {}
        }
    }

    /// 整个 run_turn 调用结束（含其后续队列执行完毕）。
    /// turn 链终态回调：复位运行相位，并在存在排队压缩时武装并返回
    /// `Action::Compact` 由事件循环 spawn 后台压缩线程。
    pub fn on_chain_finished(&mut self, result: &Result<TurnStatus, String>) -> Action {
        self.phase = Phase::Idle;
        self.set_waiting(WaitingTarget::None);
        self.quit_armed = false;
        self.turn_started_at = None;
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
        // 排队压缩在 turn 终态后自动启动（复用同一压缩路径与取消令牌）。
        if self.compact_queued {
            self.compact_queued = false;
            self.compacting = true;
            self.compact_cancel = Some(singularity_core::CancellationToken::new());
            self.transcript
                .push_note("compacting context…", NoteStyle::Dim);
            return Action::Compact;
        }
        Action::Continue
    }

    #[cfg(test)]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// 推进 spinner 节拍（由事件循环按固定间隔调用）。
    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    /// Ctrl+C：优先清空输入；输入为空时第一次确认、第二次正常退出。
    fn handle_ctrl_c(&mut self) -> Action {
        if !self.editor.is_empty() {
            self.editor.clear();
            self.quit_armed = true;
            return Action::Continue;
        }
        if self.quit_armed {
            return Action::Exit(0);
        }
        self.quit_armed = true;
        Action::Continue
    }

    /// 二次确认复位：任何非 Ctrl+C 按键、提交输入与 turn 链结束都会清除。
    fn reset_quit_confirm(&mut self) {
        self.quit_armed = false;
    }

    /// 请求中断活动 turn（Esc 与点击 [stop] 同一路径）：取消 provider
    /// 调用并转入中断收敛相位；空闲时为空操作。
    pub(super) fn request_interrupt(&mut self) {
        if self.phase == Phase::Idle {
            return;
        }
        self.conversation.interrupt();
        self.phase = Phase::Interrupting;
        self.set_waiting(WaitingTarget::TerminalConvergence);
    }

    // -- 键盘路由 ------------------------------------------------------------

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Action::Continue;
        }

        // Ctrl+C 是应用级按键语义，先于 settings 模态消费，且不受 turn
        // 相位或模态影响；其余任何按键都取消已 armed 的二次确认。
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            return self.handle_ctrl_c();
        }
        self.reset_quit_confirm();

        if self.settings.is_some() {
            return self.handle_settings_key(key.code);
        }
        if self.resume.is_some() {
            return self.handle_resume_key(key.code);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('t') if ctrl => self.transcript.toggle_thinking(),
            KeyCode::Char('j') if ctrl => self.editor.insert_newline(),
            KeyCode::Char('o') if ctrl || alt => {
                self.transcript.toggle_latest_tool_expansion();
            }
            KeyCode::Esc => {
                if self.phase == Phase::Idle {
                    let (total, viewport) = self.flow_metrics();
                    if !self.scroll.is_following() {
                        self.scroll.jump_to_bottom(total, viewport);
                    } else if !self.editor.is_empty() {
                        self.editor.clear();
                    }
                }
                self.request_interrupt();
                // 压缩进行中：Esc 取消本次压缩（与中断同一按键语义）。
                if self.compacting {
                    self.cancel_compact();
                } else if self.compact_queued {
                    self.compact_queued = false;
                    self.transcript
                        .push_note("compaction cancelled", NoteStyle::Warning);
                }
            }
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => return Action::Exit(0),
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
                if let Some(text) = self.conversation.withdraw_follow_up() {
                    self.transcript.push_note(
                        format!("withdrawn: {}", truncate_label(&text, 40)),
                        NoteStyle::Accent,
                    );
                }
            }
            KeyCode::Down if alt => {
                let (total, viewport) = self.flow_metrics();
                self.scroll.scroll_down(1, total, viewport);
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.editor.insert_newline();
            }
            KeyCode::Enter if alt => {
                if self.phase != Phase::Idle {
                    self.submit_follow_up();
                }
            }
            KeyCode::Enter => return self.submit_input(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => self.editor.move_right(),
            KeyCode::Up => self.editor.move_up(),
            KeyCode::Down => self.editor.move_down(),
            KeyCode::Home => self.editor.move_home(),
            KeyCode::End if !self.scroll.is_following() => {
                let (total, viewport) = self.flow_metrics();
                self.scroll.jump_to_bottom(total, viewport);
            }
            KeyCode::End => self.editor.move_end(),
            KeyCode::Char(ch) if !ctrl && !alt => self.editor.insert_char(ch),
            _ => {}
        }
        Action::Continue
    }

    fn submit_input(&mut self) -> Action {
        // 压缩持有一致性写窗口：完成前不接受新输入（否则误报
        // TurnAlreadyActive 一类晦涩错误）；Esc 可取消。检查必须在
        // editor.take() 之前，保证被拒时草稿保留在原处。
        if self.compacting {
            self.transcript.push_note(
                "compaction in progress; finish or press Esc to cancel",
                NoteStyle::Warning,
            );
            return Action::Continue;
        }
        let raw = self.editor.take();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return Action::Continue;
        }
        if text.starts_with('/') {
            return self.execute_command(&text);
        }
        match self.phase {
            Phase::Idle => {
                // 新回合 page-flip：视口钉在新内容首行，回复填满一屏后回底
                // 跟随（参照 Grok 的 follow_new_turn）。
                let (total, _) = self.flow_metrics();
                self.scroll.pin_new_content_at(total);
                self.phase = Phase::Running;
                self.set_waiting(WaitingTarget::Model);
                Action::Submit(text)
            }
            Phase::Running | Phase::Interrupting => {
                let accepted = self.conversation.steer(text.clone());
                self.note_injection("steer", accepted, &text);
                // steer 注入后回到最新内容（page-flip 只属于新回合）。
                let (total, viewport) = self.flow_metrics();
                self.scroll.jump_to_bottom(total, viewport);
                Action::Continue
            }
        }
    }

    // -- 渲染 ----------------------------------------------------------------

    pub(super) fn flow_metrics(&self) -> (usize, usize) {
        let width = self.last_flow_width.unwrap_or(80);
        let total: usize = self.transcript.row_counts(width).iter().sum::<usize>()
            + self.transcript.live_row_count(width);
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
        let mut counts = self.transcript.row_counts(flow.width);
        counts.push(self.transcript.live_row_count(flow.width));
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
            let finished_items = self.transcript.item_count();
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
                    let line = if item_index < finished_items {
                        self.transcript.render_item_row(
                            item_index,
                            row_in_item,
                            flow.width,
                            spinner,
                        )
                    } else {
                        self.transcript.render_live_row(row_in_item, flow.width)
                    };
                    if let Some(line) = line {
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
        // 滚轮覆盖优先，否则跟随光标。
        let scroll_top = self.editor.effective_scroll_top(visual_row, inner_h);
        self.last_editor_scroll_top = scroll_top;
        let mut editor_lines: Vec<Line<'static>> = Vec::new();
        for logical in self.editor.lines() {
            for piece in wrapped_lines(logical, editor_inner_w as usize) {
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
        frame.render_widget(
            Paragraph::new(vec![Line::from(status_spans.clone())]),
            status_area,
        );
        frame.render_widget(Paragraph::new(vec![Line::from(hint_spans)]), hint_area);

        // 点击命中缓存：登记本帧可点击矩形。[stop] 恒为状态行末段（按 span
        // 宽度计量，不做文本反查）；编辑器内区不含边框。
        self.click_targets.clear();
        if self.phase != Phase::Idle {
            let stop = status_spans
                .last()
                .expect("running footer always ends with [stop]");
            let stop_width = UnicodeWidthStr::width(stop.content.as_ref()) as u16;
            let status_width: u16 = status_spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()) as u16)
                .sum();
            let stop_x = status_area
                .x
                .saturating_add(status_width.saturating_sub(stop_width));
            self.click_targets.push((
                Rect::new(stop_x, status_area.y, stop_width, 1),
                ClickTarget::Stop,
            ));
        }
        let editor_inner = Rect {
            x: editor_area.x.saturating_add(1),
            y: editor_area.y.saturating_add(1),
            width: editor_area.width.saturating_sub(2).max(1),
            height: editor_area.height.saturating_sub(2).max(1),
        };
        self.click_targets.push((editor_inner, ClickTarget::Editor));

        if let Some(menu) = &self.settings {
            self.render_settings(frame, menu);
        } else if let Some(menu) = &self.resume {
            self.render_resume(frame, menu);
        } else if self.editor.text().starts_with('/')
            && !self.editor.text().contains(char::is_whitespace)
        {
            self.render_command_menu(frame);
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
    fn editor_text(&self) -> String {
        self.editor.text()
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;

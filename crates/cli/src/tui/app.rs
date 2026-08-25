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
use unicode_width::UnicodeWidthStr;

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

const COMMANDS: [(&str, &str); 7] = [
    ("/model", "select the thread model"),
    ("/settings", "edit provider, model, and reasoning"),
    ("/resume", "resume a saved session"),
    ("/new", "start a new session"),
    ("/session", "show session facts"),
    ("/compact", "compact context now"),
    ("/name", "name this session"),
];

/// 设置菜单提示：菜单内与状态行提示共用同一文案（行为与提示同源，防漂移）。
const SETTINGS_MENU_HINT: &str = "Enter apply · Tab next field · Esc close";

/// 滚轮归一化：按事件间隔区分滚轮/触控板并区间加速（参照 Grok 的
/// `mouse.rs` 简化版——<8ms ×2.5、<20ms ×1.6，其余 ×1.0），小数部分
/// 累计到下一事件，单次事件有上下限防失控。
#[derive(Default)]
pub(crate) struct WheelNormalizer {
    last: Option<std::time::Instant>,
    pending: f64,
}

impl WheelNormalizer {
    fn rows_for(&mut self, now: std::time::Instant) -> usize {
        let multiplier = match self.last {
            Some(last) => {
                let gap_ms = now.duration_since(last).as_millis();
                if gap_ms <= 8 {
                    2.5
                } else if gap_ms <= 20 {
                    1.6
                } else {
                    1.0
                }
            }
            None => 1.0,
        };
        self.last = Some(now);
        self.pending += WHEEL_ROWS as f64 * multiplier;
        let rows = self.pending.floor() as usize;
        self.pending -= rows as f64;
        rows.clamp(1, 8)
    }
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
    fn label(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Model => Some("model".to_string()),
            Self::Thinking => Some("thinking".to_string()),
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
        Self::open_field(current_model, 0)
    }

    fn open_field(current_model: Option<&str>, field: usize) -> Self {
        let parts = singularity_model::split_model_selector(current_model.unwrap_or_default());
        Self {
            field,
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

pub(crate) struct ResumeMenu {
    threads: Vec<singularity_runtime::ThreadSummary>,
    selected: usize,
}

/// 键盘处理结果：继续、提交一轮输入或以指定退出码结束进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Continue,
    Submit(String),
    Exit(i32),
}

/// 交互式会话的应用状态。
pub(crate) struct TuiApp {
    conversation: Arc<Conversation>,
    transcript: Transcript,
    scroll: ScrollState,
    editor: Editor,
    phase: Phase,
    waiting: WaitingTarget,
    settings: Option<SettingsMenu>,
    resume: Option<ResumeMenu>,
    thread_id: String,
    /// 二次确认退出已生效：下一次 Ctrl+C 直接退出（空闲 0 / 运行中 130）。
    /// 复位规则：任何非 Ctrl+C 按键、提交输入或 turn 链结束都会清除；
    /// 按下期间提示行持续显示再次确认文案。
    quit_armed: bool,
    spinner_frame: usize,
    /// 当前等待对象开始等待的时刻（状态行相位计时）。
    waiting_since: Option<std::time::Instant>,
    turn_started_at: Option<std::time::Instant>,
    last_usage: Option<singularity_runtime::TurnUsage>,
    /// 最近一帧会话流宽度、总行数与视口高（键位滚动与增长检测依赖）。
    last_flow_width: Option<u16>,
    last_total_rows: usize,
    last_viewport_rows: usize,
    last_editor_area: Option<Rect>,
    last_editor_scroll_top: usize,
    last_status_area: Option<Rect>,
    /// 状态行 "[stop]" 的可点击列范围（终端坐标；空闲或未渲染时为 None）。
    last_stop_cols: Option<(u16, u16)>,
    /// 滚轮归一化状态（滚轮/触控板加速）。
    wheel: WheelNormalizer,
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
            last_usage: None,
            last_flow_width: None,
            last_total_rows: 0,
            last_viewport_rows: 5,
            last_editor_area: None,
            last_editor_scroll_top: 0,
            last_status_area: None,
            last_stop_cols: None,
            wheel: WheelNormalizer::default(),
        }
    }

    pub fn conversation_handle(&self) -> Arc<Conversation> {
        Arc::clone(&self.conversation)
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
                self.turn_started_at = Some(std::time::Instant::now());
            }
            TurnEvent::AssistantDelta { delta, item_id, .. } => {
                if Self::is_assistant(item_id) {
                    self.transcript.assistant_delta(delta);
                }
                self.set_waiting(WaitingTarget::Model);
            }
            TurnEvent::ProviderAttempt { status, .. } => {
                if status == "started" {
                    self.set_waiting(WaitingTarget::Thinking);
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
                self.last_usage = turn.usage.clone();
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
            // 待生效设置已在可信终态后应用：状态行与后续 turn 自会反映新
            // selector，无需额外文案（提交点已提示过生效时点）。
            TurnEvent::ThreadSettingsApplied { .. } => {}
        }
    }

    fn is_assistant(item_id: &str) -> bool {
        item_id.ends_with("_assistant")
    }

    /// 整个 run_turn 调用结束（含其后续队列执行完毕）。
    pub fn on_chain_finished(&mut self, result: &Result<TurnStatus, String>) {
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

    // -- 键盘路由 ------------------------------------------------------------

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Action::Continue;
        }

        // Ctrl+C 是应用级按键语义，先于 settings 模态消费：行为只由 turn
        // 相位决定，模态不改变它；其余任何按键都取消已 armed 的二次确认。
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            return self.handle_ctrl_c();
        }
        self.reset_quit_confirm();

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
                        Ok(singularity_runtime::SettingsApplyTiming::NothingToApply) => {
                            menu.error = Some("nothing to change".into());
                        }
                        Ok(timing) => {
                            let queued_now = timing
                                == singularity_runtime::SettingsApplyTiming::QueuedForNextTurn;
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
                        Err(error) => menu.error = Some(error.to_string()),
                    }
                }
                KeyCode::Char(ch) => menu.current_mut().push(ch),
                _ => {}
            }
            return Action::Continue;
        }

        if let Some(menu) = self.resume.as_mut() {
            match key.code {
                KeyCode::Esc => self.resume = None,
                KeyCode::Up => menu.selected = menu.selected.saturating_sub(1),
                KeyCode::Down => {
                    menu.selected = (menu.selected + 1).min(menu.threads.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    let selected = menu.threads.get(menu.selected).cloned();
                    self.resume = None;
                    if let Some(summary) = selected {
                        self.resume_thread(&summary.thread_id);
                    }
                }
                _ => {}
            }
            return Action::Continue;
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
                if self.phase != Phase::Idle {
                    self.conversation.interrupt();
                    self.phase = Phase::Interrupting;
                    self.set_waiting(WaitingTarget::TerminalConvergence);
                } else {
                    let (total, viewport) = self.flow_metrics();
                    if !self.scroll.is_following() {
                        self.scroll.jump_to_bottom(total, viewport);
                    } else if !self.editor.is_empty() {
                        self.editor.clear();
                    }
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

    /// 鼠标滚轮：指针在输入框内时滚动编辑器视口（光标一动即回跟随），
    /// 其余滚动会话流；事件间隔触发滚轮/触控板加速（参照 Grok 的滚轮路由）。
    pub fn handle_wheel(&mut self, up: bool, column: u16, row: u16) {
        let rows = self.wheel.rows_for(std::time::Instant::now());
        if let Some(area) = self.last_editor_area {
            let inside_x = column > area.x && column < area.x.saturating_add(area.width - 1);
            let inside_y = row > area.y && row < area.y.saturating_add(area.height - 1);
            if inside_x && inside_y {
                self.editor
                    .scroll_by(if up { -(rows as i32) } else { rows as i32 });
                return;
            }
        }
        let (total, viewport) = self.flow_metrics();
        if up {
            self.scroll.scroll_up(rows, total, viewport);
        } else {
            self.scroll.scroll_down(rows, total, viewport);
        }
    }

    pub fn handle_click(&mut self, column: u16, row: u16) {
        // 运行中点击状态行 "[stop]"：中断当前轮（与 Esc 同一路径）。
        if self.phase != Phase::Idle
            && let Some(area) = self.last_status_area
            && let Some((start, end)) = self.last_stop_cols
            && row == area.y
            && column >= start
            && column < end
        {
            self.conversation.interrupt();
            self.phase = Phase::Interrupting;
            self.set_waiting(WaitingTarget::TerminalConvergence);
            return;
        }
        let Some(area) = self.last_editor_area else {
            return;
        };
        let inside_x = column > area.x && column < area.x.saturating_add(area.width - 1);
        let inside_y = row > area.y && row < area.y.saturating_add(area.height - 1);
        if !inside_x || !inside_y {
            return;
        }
        let visual_row = self
            .last_editor_scroll_top
            .saturating_add((row - area.y - 1) as usize);
        let visual_col = (column - area.x - 1) as usize;
        self.editor
            .set_cursor_visual(visual_row, visual_col, area.width.saturating_sub(2));
    }

    fn resume_thread(&mut self, thread_id: &str) {
        let runner = self.conversation.runner_handle();
        match singularity_runtime::resume_thread(runner.sessions_dir(), thread_id) {
            Ok(thread) => {
                self.conversation = Conversation::new(runner, thread.clone());
                self.thread_id = thread.thread_id;
                self.transcript = Transcript::new();
                self.scroll = ScrollState::default();
                self.last_usage =
                    singularity_runtime::list_threads(self.conversation.runner().sessions_dir())
                        .ok()
                        .and_then(|threads| {
                            threads
                                .into_iter()
                                .find(|summary| summary.thread_id == self.thread_id)
                        })
                        .map(|summary| singularity_runtime::TurnUsage {
                            input_tokens: 0,
                            output_tokens: 0,
                            total_tokens: summary.total_tokens,
                            cached_input_tokens: 0,
                            reasoning_tokens: 0,
                            usage_present: summary.total_tokens > 0,
                            usage_complete: false,
                        });
                self.transcript.push_note(
                    format!("resumed thread {}", short_id(&self.thread_id)),
                    NoteStyle::Accent,
                );
            }
            Err(error) => self
                .transcript
                .push_note(format!("resume failed: {error}"), NoteStyle::Error),
        }
    }

    fn execute_command(&mut self, text: &str) {
        let (command, argument) = text.split_once(' ').unwrap_or((text, ""));
        match command {
            "/model" => {
                let current = self
                    .conversation
                    .thread()
                    .ok()
                    .and_then(|thread| thread.model);
                self.settings = Some(SettingsMenu::open_field(current.as_deref(), 1));
            }
            "/settings" => {
                let current = self
                    .conversation
                    .thread()
                    .ok()
                    .and_then(|thread| thread.model);
                self.settings = Some(SettingsMenu::open(current.as_deref()));
            }
            "/resume" => {
                match singularity_runtime::list_threads(self.conversation.runner().sessions_dir()) {
                    Ok(threads) if !threads.is_empty() => {
                        self.resume = Some(ResumeMenu {
                            threads,
                            selected: 0,
                        });
                    }
                    Ok(_) => self
                        .transcript
                        .push_note("no saved sessions", NoteStyle::Dim),
                    Err(error) => self.transcript.push_note(error, NoteStyle::Error),
                }
            }
            "/new" => {
                let runner = self.conversation.runner_handle();
                let current = self.conversation.thread().ok();
                let cwd = current
                    .as_ref()
                    .map(|thread| thread.cwd.clone())
                    .unwrap_or_default();
                let model = current.and_then(|thread| thread.model);
                match singularity_runtime::create_thread(runner.sessions_dir(), &cwd, model) {
                    Ok(thread) => {
                        let thread_id = thread.thread_id.clone();
                        self.conversation = Conversation::new(runner, thread);
                        self.thread_id = thread_id;
                        self.transcript = Transcript::new();
                        self.scroll = ScrollState::default();
                        self.last_usage = None;
                        self.transcript.push_note(
                            format!("new thread {}", short_id(&self.thread_id)),
                            NoteStyle::Accent,
                        );
                    }
                    Err(error) => self.transcript.push_note(error, NoteStyle::Error),
                }
            }
            "/session" => {
                let summary =
                    singularity_runtime::list_threads(self.conversation.runner().sessions_dir())
                        .ok()
                        .and_then(|threads| {
                            threads
                                .into_iter()
                                .find(|summary| summary.thread_id == self.thread_id)
                        });
                match summary {
                    Some(summary) => self.transcript.push_note(
                        format!(
                            "session {} · {} turns · {} tokens",
                            summary.thread_id, summary.turn_count, summary.total_tokens
                        ),
                        NoteStyle::Accent,
                    ),
                    None => self
                        .transcript
                        .push_note("session facts unavailable", NoteStyle::Warning),
                }
            }
            "/compact" => match self.conversation.compact() {
                Ok(singularity_runtime::CompactionOutcome::Compacted { tokens_before, .. }) => {
                    self.transcript.push_note(
                        format!("context compacted from {tokens_before} estimated tokens"),
                        NoteStyle::Accent,
                    )
                }
                Ok(singularity_runtime::CompactionOutcome::NotNeeded) => self
                    .transcript
                    .push_note("nothing to compact", NoteStyle::Dim),
                Err(error) => self
                    .transcript
                    .push_note(format!("compaction failed: {error}"), NoteStyle::Error),
            },
            "/name" if !argument.trim().is_empty() => {
                match self.conversation.rename(argument.trim()) {
                    Ok(()) => self.transcript.push_note(
                        format!("session named {}", argument.trim()),
                        NoteStyle::Accent,
                    ),
                    Err(error) => self
                        .transcript
                        .push_note(error.to_string(), NoteStyle::Error),
                }
            }
            "/name" => self
                .transcript
                .push_note("usage: /name <session name>", NoteStyle::Warning),
            _ => self
                .transcript
                .push_note(format!("unknown command: {command}"), NoteStyle::Warning),
        }
    }

    fn submit_follow_up(&mut self) {
        let text = self.editor.take().trim().to_string();
        if text.is_empty() {
            return;
        }
        let accepted = self.conversation.submit_follow_up(text.clone());
        self.note_injection("followUp", accepted, &text);
    }

    fn submit_input(&mut self) -> Action {
        let raw = self.editor.take();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return Action::Continue;
        }
        if text.starts_with('/') {
            self.execute_command(&text);
            return Action::Continue;
        }
        self.reset_quit_confirm();
        match self.phase {
            Phase::Idle => {
                // 新回合 page-flip：视口钉在新内容首行，回复填满一屏后回底
                // 跟随（参照 Grok 的 follow_new_turn）。
                let (total, _) = self.flow_metrics();
                self.scroll.pin_new_content_at(total);
                self.phase = Phase::Running;
                self.waiting = WaitingTarget::Model;
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
        self.last_editor_area = Some(editor_area);
        self.last_editor_scroll_top = scroll_top;
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
        self.last_status_area = Some(status_area);
        self.last_stop_cols = stop_span_columns(&status_spans, status_area.x);
        frame.render_widget(Paragraph::new(vec![Line::from(status_spans)]), status_area);
        frame.render_widget(Paragraph::new(vec![Line::from(hint_spans)]), hint_area);

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
            SETTINGS_MENU_HINT,
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

    fn render_resume(&self, frame: &mut Frame<'_>, menu: &ResumeMenu) {
        let height = (menu.threads.len().min(8) as u16).saturating_add(2).max(3);
        let popup = centered_rect(frame.area(), 72, height);
        frame.render_widget(Clear, popup);
        let lines = menu
            .threads
            .iter()
            .take(8)
            .enumerate()
            .map(|(index, thread)| {
                let style = if index == menu.selected {
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::DarkGray)
                };
                Line::from(Span::styled(
                    format!(
                        "{} · {} turns · {} tokens · {}",
                        short_id(&thread.thread_id),
                        thread.turn_count,
                        thread.total_tokens,
                        thread.title.as_deref().unwrap_or("untitled")
                    ),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("resume")),
            popup,
        );
    }

    fn render_command_menu(&self, frame: &mut Frame<'_>) {
        let prefix = self.editor.text();
        let matches = COMMANDS
            .iter()
            .filter(|(command, _)| command.starts_with(&prefix))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return;
        }
        let popup = centered_rect(frame.area(), 64, matches.len() as u16 + 2);
        frame.render_widget(Clear, popup);
        let lines = matches
            .into_iter()
            .map(|(command, description)| {
                Line::from(vec![
                    Span::styled(format!("{command:<12}"), Style::new().fg(Color::Cyan)),
                    Span::styled(*description, Style::new().fg(Color::DarkGray)),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("commands")),
            popup,
        );
    }

    /// footer 合同：状态行＝相位+spinner·具名等待对象·thread·模型·
    /// token/队列数·浏览指示（含新增计数）。提示行按上下文给出关键操作。
    pub fn footer_spans(
        &self,
        total_rows: usize,
        viewport: usize,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let dim = Style::new().fg(Color::DarkGray);
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
            let turn_elapsed = self
                .turn_started_at
                .map(|started| started.elapsed().as_secs())
                .unwrap_or(0);
            status.push(Span::styled(format!(" · turn {turn_elapsed}s"), warn));
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
        if self.transcript.thinking_collapsed() {
            status.push(Span::styled("[thinking folded]", dim));
        }
        if let Some(usage) = &self.last_usage
            && usage.usage_present
        {
            status.push(Span::styled(format!(" {} tokens", usage.total_tokens), dim));
        }
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
        // 运行中显示可点击的 [stop]（点击 = 中断，参照 Grok 的 turn-status）。
        if self.phase != Phase::Idle {
            status.push(Span::styled(
                "[stop]",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }

        let hint_text = if self.quit_armed {
            "press Ctrl+C again to quit"
        } else if self.settings.is_some() {
            SETTINGS_MENU_HINT
        } else if self.resume.is_some() {
            "↑/↓ select · Enter resume · Esc close"
        } else {
            match self.phase {
                Phase::Idle => {
                    "Enter send · Ctrl+J newline · / commands · PgUp/PgDn scroll · End latest"
                }
                Phase::Running | Phase::Interrupting => {
                    "Enter steer · Alt+Enter queue · Alt+Up withdraw · Esc stop · Ctrl+T thinking · Ctrl+O tool view"
                }
            }
        };
        let hint_style = if self.quit_armed {
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

/// 状态行中 "[stop]" 可点击区域的列范围（终端坐标；未渲染时返回 None）。
fn stop_span_columns(spans: &[Span<'_>], origin_x: u16) -> Option<(u16, u16)> {
    let mut col = origin_x;
    for span in spans {
        let width = UnicodeWidthStr::width(span.content.as_ref()) as u16;
        if span.content.as_ref() == "[stop]" {
            return Some((col, col + width));
        }
        col += width;
    }
    None
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

        for ch in "/settings".chars() {
            app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
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

        // 与生产路径一致：编辑器输入 + Alt+Enter 提交 followUp。
        for ch in "second".chars() {
            app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(app.editor_text(), "second");
        app.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
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

    // -- Ctrl+C 状态机（按键驱动） -------------------------------------------

    fn ctrl_c() -> KeyEvent {
        key(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn hint_text(app: &TuiApp) -> String {
        let (_, hint) = app.footer_spans(100, 20);
        hint.iter().map(|span| span.content.clone()).collect()
    }

    #[test]
    fn idle_first_ctrl_c_arms_confirm_and_second_exits_zero() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        assert_eq!(app.handle_key(ctrl_c()), Action::Continue);
        assert_eq!(
            app.phase(),
            Phase::Idle,
            "idle Ctrl+C must not change phase"
        );
        assert!(
            hint_text(&app).contains("again to quit"),
            "idle first Ctrl+C shows the re-confirm hint"
        );
        assert_eq!(
            app.handle_key(ctrl_c()),
            Action::Exit(0),
            "idle second Ctrl+C exits with code 0"
        );
    }

    #[test]
    fn running_ctrl_c_uses_the_normal_two_press_exit() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        app.force_phase(Phase::Running);
        assert_eq!(app.handle_key(ctrl_c()), Action::Continue);
        assert_eq!(
            app.phase(),
            Phase::Running,
            "Ctrl+C does not interrupt the active turn"
        );
        assert!(
            hint_text(&app).contains("again to quit"),
            "the first Ctrl+C announces the normal exit confirmation"
        );
        assert_eq!(
            app.handle_key(ctrl_c()),
            Action::Exit(0),
            "the second Ctrl+C exits normally"
        );
    }

    #[test]
    fn running_escape_delivers_interrupt_to_the_active_turn() {
        use singularity_core::CancellationToken;
        struct ProbeProvider {
            probe: Arc<std::sync::Mutex<Option<CancellationToken>>>,
        }
        impl singularity_model::Provider for ProbeProvider {
            fn protocol_contract(&self) -> singularity_model::ProviderProtocolContract {
                singularity_model::ProviderProtocolContract::default()
            }
            fn complete(
                &self,
                _request: &singularity_model::ModelTurnRequest,
                cancellation: &CancellationToken,
            ) -> Result<singularity_model::ModelTurnResponse, singularity_model::ProviderError>
            {
                self.probe.lock().unwrap().replace(cancellation.clone());
                while !cancellation.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(singularity_model::ProviderError::from_model_error(
                    singularity_model::ModelError::new(
                        singularity_model::ModelErrorKind::Cancelled,
                        "cancelled",
                    ),
                ))
            }
        }

        let (_home, sessions) = test_home();
        let probe: Arc<std::sync::Mutex<Option<CancellationToken>>> = Arc::default();
        let conversation = test_conversation(
            &sessions,
            Arc::new(ProbeProvider {
                probe: Arc::clone(&probe),
            }),
        );
        let mut app = TuiApp::new(Arc::clone(&conversation));
        let (tx, rx) = mpsc::channel::<crate::tui::UiEvent>();
        for ch in "block me".chars() {
            app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Submit(goal) => crate::tui::spawn_turn(&conversation, goal, tx.clone()),
            other => panic!("expected Submit action, got {other:?}"),
        }
        for _ in 0..400 {
            if probe.lock().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(probe.lock().unwrap().is_some(), "provider must be running");

        // 生产路径：运行中 Esc → Conversation::interrupt 送达当前轮。
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.phase(), Phase::Interrupting);
        assert!(
            probe.lock().unwrap().as_ref().unwrap().is_cancelled(),
            "Esc must cancel the active turn token"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(16)) {
                Ok(crate::tui::UiEvent::ChainFinished(Ok(TurnStatus::Interrupted))) => break,
                Ok(_) => {}
                Err(_) => panic!("chain should finish interrupted"),
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for interrupted chain finish");
            }
        }
    }

    // -- 鼠标与滚轮 ----------------------------------------------------------

    #[test]
    fn clicking_stop_interrupts_the_running_turn() {
        let (_home, sessions) = test_home();
        let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
        app.force_phase(Phase::Running);
        let status_area = ratatui::layout::Rect::new(0, 28, 100, 1);
        app.last_status_area = Some(status_area);
        app.last_stop_cols = Some((90, 97));
        // 命中 [stop]：中断当前轮。
        app.handle_click(93, status_area.y);
        assert_eq!(
            app.phase(),
            Phase::Interrupting,
            "click on [stop] interrupts"
        );
    }

    // -- page-flip 提交 ------------------------------------------------------
}

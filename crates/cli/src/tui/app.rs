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
use singularity_runtime::events::{AgentDiagnosticSeverity, ProviderAttemptStatus, TurnEvent};
use singularity_runtime::objects::TurnStatus;
use singularity_runtime::{Conversation, ThreadCatalog};
use unicode_width::UnicodeWidthStr;

use super::commands::Action;
use super::editor::Editor;
use super::history::InputHistory;
use super::modals::{ResumeMenu, SettingsMenu};
use super::mouse::{ClickTarget, WheelNormalizer};
use super::paste_burst::{CharDecision, FlushResult, PasteBurst};
use super::scroll::ScrollState;
use super::transcript::{NoteStyle, Transcript};
use super::view::{describe_usage, short_id, truncate_label};
use super::wrapped_lines;

pub(super) const SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];
const MAX_EDITOR_ROWS_CAP: u16 = 10;
/// 单次粘贴的字节上限：防止超大粘贴拖垮折行渲染；按整字符边界截断。
const MAX_PASTE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Running,
}

/// /compact 的唯一状态源。排队与运行态互斥，运行态自带本次操作的取消令牌。
#[derive(Debug, Default)]
pub(super) enum CompactionState {
    #[default]
    Idle,
    Queued,
    Running(singularity_core::CancellationToken),
}

impl CompactionState {
    pub(super) fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }

    pub(super) fn is_queued(&self) -> bool {
        matches!(self, Self::Queued)
    }

    pub(super) fn queue(&mut self) {
        debug_assert!(matches!(self, Self::Idle));
        *self = Self::Queued;
    }

    pub(super) fn start(&mut self) -> singularity_core::CancellationToken {
        debug_assert!(!self.is_running());
        let cancellation = singularity_core::CancellationToken::new();
        *self = Self::Running(cancellation.clone());
        cancellation
    }

    pub(super) fn start_if_queued(&mut self) -> Option<singularity_core::CancellationToken> {
        self.is_queued().then(|| self.start())
    }

    pub(super) fn finish(&mut self) -> bool {
        let previous = std::mem::take(self);
        matches!(previous, Self::Running(token) if token.is_cancelled())
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

/// 单帧渲染缓存：会话流宽/行数/视口度量与点击命中矩形表。帧间存活，
/// 供键位滚动、内容增长检测与鼠标命中复用。
pub(super) struct FrameCache {
    pub(super) last_flow_width: Option<u16>,
    pub(super) last_total_rows: usize,
    pub(super) last_viewport_rows: usize,
    /// 编辑器最近一帧的可视滚动顶行（点击定位换算依赖）。
    pub(super) last_editor_scroll_top: usize,
    /// 帧缓存点击矩形表：本帧渲染时登记 `(Rect, ClickTarget)` 对，鼠标
    /// 事件对缓存做包含测试（取代对状态行文本的反查）。
    pub(super) click_targets: Vec<(Rect, ClickTarget)>,
}

impl Default for FrameCache {
    fn default() -> Self {
        Self {
            last_flow_width: None,
            last_total_rows: 0,
            last_viewport_rows: 5,
            last_editor_scroll_top: 0,
            click_targets: Vec::new(),
        }
    }
}

/// 交互式会话的应用状态。字段以 `pub(super)` 暴露给同模块树中的
/// `modals`/`session_actions`/`mouse`/`view`（同 `tui` 模块内部实现细节）。
pub(crate) struct TuiApp {
    pub(super) conversation: Arc<Conversation>,
    pub(super) thread_catalog: ThreadCatalog,
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
    pub(super) frame: FrameCache,
    /// 滚轮归一化状态（滚轮/触控板加速）。
    pub(super) wheel: WheelNormalizer,
    /// /compact 的排队、运行和取消状态。
    pub(super) compaction: CompactionState,
    /// 无括号粘贴终端的 burst 检测状态。
    pub(super) paste_burst: PasteBurst,
    /// burst 检测开关：真实终端默认启用；PTY 集成测试通过
    /// `SINGULARITY_DISABLE_PASTE_BURST` 关闭，避免瞬时注入整串被当作
    /// 粘贴（参照 codex 的 disable_paste_burst 逃生舱）。
    pub(super) paste_burst_enabled: bool,
    /// 会话内历史（不持久化）：逐条记录提交文本，供 ↑/↓ 回溯。
    history: InputHistory,
    /// 进入回溯前暂存的草稿，退出回溯且未编辑时恢复。
    history_draft: Option<String>,
    /// 回溯期间是否发生过编辑（字符插入/删除/粘贴/Enter 等），用于决定
    /// 退出时是否恢复草稿。
    history_edited: bool,
}

impl TuiApp {
    pub fn new(conversation: Arc<Conversation>) -> Self {
        let thread_catalog = ThreadCatalog::new(conversation.runner_handle().as_ref());
        let thread_id = conversation
            .thread()
            .map(|thread| thread.thread_id)
            .unwrap_or_default();
        Self {
            conversation,
            thread_catalog,
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
            frame: FrameCache::default(),
            wheel: WheelNormalizer::default(),
            compaction: CompactionState::default(),
            paste_burst: PasteBurst::default(),
            paste_burst_enabled: std::env::var_os("SINGULARITY_DISABLE_PASTE_BURST").is_none(),
            history: InputHistory::new(),
            history_draft: None,
            history_edited: false,
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
                    format!("── turn {} ──", short_id(&turn.turn_id)),
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
                    self.transcript
                        .tool_terminal(item_id, matches!(event, TurnEvent::ItemFailed { .. }));
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
        if let Some(cancellation) = self.compaction.start_if_queued() {
            self.transcript
                .push_note("compacting context…", NoteStyle::Dim);
            return Action::Compact(cancellation);
        }
        Action::Continue
    }

    /// 推进 spinner 节拍（由事件循环按固定间隔调用）。
    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    /// Ctrl+C：优先清空输入；输入为空时第一次确认、第二次正常退出。
    fn handle_ctrl_c(&mut self) -> Action {
        // 未落地的 burst 缓冲一并清空：Ctrl+C 语义是丢弃当前输入。
        self.paste_burst.clear_after_explicit_paste();
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
    /// 调用并等待收敛；空闲时为空操作。
    pub(super) fn request_interrupt(&mut self) {
        if self.phase == Phase::Idle {
            return;
        }
        self.conversation.interrupt();
        self.set_waiting(WaitingTarget::TerminalConvergence);
    }

    // -- 键盘路由 ------------------------------------------------------------

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Action::Continue;
        }

        // 任何按键处理前先冲刷已到期的 burst 暂存/缓冲：暂存字符在静默
        // 超时后作为普通输入落地，避免被后续按键顶替而丢失。
        self.flush_paste_burst_if_due(std::time::Instant::now());

        // Ctrl+C 是应用级按键语义，先于 settings 模态消费，且不受 turn
        // 相位或模态影响；其余任何按键都取消已 armed 的二次确认。
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            return self.handle_ctrl_c();
        }
        self.reset_quit_confirm();

        if self.settings.is_some() {
            return self.handle_settings_key(key);
        }
        if self.resume.is_some() {
            return self.handle_resume_key(key);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // 非文本输入（含修饰快捷键、方向键等）前先落地未超时的 burst 缓冲，
        // 避免残留文本卡住或被并入后续按键。
        if self.paste_burst_enabled
            && !matches!(key.code, KeyCode::Char(_) | KeyCode::Enter)
            && let Some(pasted) = self.paste_burst.flush_before_modified_input()
        {
            self.handle_paste(pasted);
        }
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
                if self.compaction.is_running() {
                    self.cancel_compact();
                } else if self.compaction.is_queued() {
                    self.compaction = CompactionState::Idle;
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
            KeyCode::Enter => {
                let now = std::time::Instant::now();
                // burst 抑制窗口内（含多行粘贴末尾的 Enter）按换行处理，
                // 不触发提交。
                if self.paste_burst_enabled
                    && self
                        .paste_burst
                        .newline_should_insert_instead_of_submit(now)
                {
                    if !self.paste_burst.append_newline_if_active(now) {
                        self.editor.insert_newline();
                    }
                    return Action::Continue;
                }
                return self.submit_input();
            }
            KeyCode::Backspace => {
                self.exit_history_after_edit();
                self.editor.backspace();
            }
            KeyCode::Delete => {
                self.exit_history_after_edit();
                self.editor.delete();
            }
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => self.editor.move_right(),
            KeyCode::Up => {
                // 历史回溯消歧：Idle 且光标在可视首行起始时 ↑ 进入回溯，
                // 否则 move_up（多行编辑不受影响）；回溯中 ↑ 上一条。
                if self.handle_history_up() {
                    return Action::Continue;
                }
                self.editor.move_up();
            }
            KeyCode::Down => {
                // 回溯中 ↓ 下一条；已到最新时退出回溯并恢复草稿，否则
                // 交还普通 move_down。
                if self.handle_history_down() {
                    return Action::Continue;
                }
                self.editor.move_down();
            }
            KeyCode::Home => self.editor.move_home(),
            KeyCode::End if !self.scroll.is_following() => {
                let (total, viewport) = self.flow_metrics();
                self.scroll.jump_to_bottom(total, viewport);
            }
            KeyCode::End => self.editor.move_end(),
            KeyCode::Char(ch) if !ctrl && !alt => {
                self.exit_history_after_edit();
                let now = std::time::Instant::now();
                self.handle_plain_char(ch, now);
            }
            _ => {}
        }
        // 非文本输入后清除 burst 计时窗口，避免下一按键并入上一 burst。
        if !matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Delete
        ) {
            self.paste_burst.clear_window_after_non_char();
        }
        Action::Continue
    }

    /// 单个无修饰字符：交给 burst 检测，缓冲/回抓/普通插入由决策驱动。
    fn handle_plain_char(&mut self, ch: char, now: std::time::Instant) {
        if !self.paste_burst_enabled {
            self.editor.insert_char(ch);
            return;
        }
        let decision = if ch.is_ascii() {
            self.paste_burst.on_plain_char(ch, now)
        } else {
            match self.paste_burst.on_plain_char_no_hold(now) {
                Some(decision) => decision,
                None => {
                    self.editor.insert_char(ch);
                    return;
                }
            }
        };
        match decision {
            CharDecision::RetainFirstChar => {
                // 首个快速字符暂存，等待下一个字符决定是输入还是 burst。
            }
            CharDecision::BufferAppend => {
                self.paste_burst.append_char_to_buffer(ch, now);
            }
            CharDecision::BeginBufferFromPending => {
                self.paste_burst.append_char_to_buffer(ch, now);
            }
            CharDecision::BeginBuffer { retro_chars } => {
                // 已按普通输入插入的前缀像粘贴：回抓进缓冲，避免内容重复
                // 与中途渲染。
                let before = self.editor.text_before_cursor();
                if let Some((_, grabbed)) =
                    self.paste_burst
                        .decide_begin_buffer(now, &before, retro_chars as usize)
                {
                    self.editor
                        .delete_chars_before_cursor(grabbed.chars().count());
                    self.paste_burst.append_char_to_buffer(ch, now);
                } else {
                    self.editor.insert_char(ch);
                    self.paste_burst.clear_window_after_non_char();
                }
            }
        }
    }

    /// 显式粘贴（bracketed paste 事件）：CRLF/CR 归一 + 字节上限整字符
    /// 截断，然后整段插入编辑器。超限在提示行警告。
    pub fn handle_paste(&mut self, text: String) {
        self.exit_history_after_edit();
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let (accepted, truncated) = truncate_utf8_chars(&text, MAX_PASTE_BYTES);
        if truncated {
            self.transcript.push_note(
                "paste exceeds 1 MiB; content truncated to a whole-character boundary",
                NoteStyle::Warning,
            );
        }
        self.editor.insert_str(&accepted);
        // 显式粘贴后清空 burst 状态，防止残留窗口影响后续输入。
        self.paste_burst.clear_after_explicit_paste();
    }

    /// 按 burst 静默超时 flush：缓冲整体作为一次粘贴，暂存字符作为普通
    /// 输入。事件循环每帧调用。
    pub fn flush_paste_burst_if_due(&mut self, now: std::time::Instant) {
        if !self.paste_burst_enabled {
            return;
        }
        match self.paste_burst.flush_if_due(now) {
            FlushResult::Paste(text) => self.handle_paste(text),
            FlushResult::Typed(ch) => self.editor.insert_char(ch),
            FlushResult::None => {}
        }
    }

    // -- 输入历史回溯 ---------------------------------------------------------

    /// ↑ 处理：回溯中上一条；Idle 且光标在首行起始时进入回溯；否则
    /// 返回 false 由调用方执行 move_up。
    fn handle_history_up(&mut self) -> bool {
        if self.history.is_navigating() {
            if let Some(text) = self.history.up() {
                self.editor.set_text(text);
            }
            return true;
        }
        // 消歧：仅 Idle 且光标在可视首行起始时 ↑ 进入回溯。
        if self.phase == Phase::Idle
            && self.editor.row() == 0
            && self.editor.col() == 0
            && !self.history.is_empty()
        {
            self.history_draft = Some(self.editor.text());
            self.history_edited = false;
            if let Some(text) = self.history.enter(self.editor.text()) {
                self.editor.set_text(text);
            }
            return true;
        }
        false
    }

    /// ↓ 处理：回溯中下一条，到最新后退出回溯并恢复草稿；否则返回
    /// false 由调用方执行 move_down。
    fn handle_history_down(&mut self) -> bool {
        if !self.history.is_navigating() {
            return false;
        }
        if let Some(text) = self.history.down() {
            self.editor.set_text(text);
            return true;
        }
        // 已到最新并退出回溯。
        if !self.history_edited
            && let Some(draft) = self.history.take_draft()
        {
            self.editor.set_text(&draft);
        }
        self.history_draft = None;
        true
    }

    /// 回溯中发生编辑（插入/删除/粘贴/Enter 提交）：退出回溯，保留
    /// 当前内容（草稿丢弃）。
    pub(super) fn exit_history_after_edit(&mut self) {
        if self.history.is_navigating() {
            self.history.exit_keeping();
            self.history_draft = None;
            self.history_edited = true;
        }
    }

    /// 记录一条提交到历史，复位回溯指针与草稿。
    pub(super) fn record_history(&mut self, text: &str) {
        self.history.record(text);
        self.history_draft = None;
        self.history_edited = false;
    }

    fn submit_input(&mut self) -> Action {
        // 压缩持有一致性写窗口：完成前不接受新输入（否则误报
        // TurnAlreadyActive 一类晦涩错误）；Esc 可取消。检查必须在
        // editor.take() 之前，保证被拒时草稿保留在原处。
        if self.compaction.is_running() {
            self.transcript.push_note(
                "compaction in progress; finish or press Esc to cancel",
                NoteStyle::Warning,
            );
            return Action::Continue;
        }
        // 提交即取走全部输入：未落地的 burst 缓冲一并清空，避免残留进入
        // 下一次编辑会话。
        self.paste_burst.clear_after_explicit_paste();
        self.exit_history_after_edit();
        let raw = self.editor.take();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return Action::Continue;
        }
        // 提交即进入历史（含命令与 steer/followUp 路径），指针复位。
        self.record_history(&text);
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
            Phase::Running => {
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
        let width = self.frame.last_flow_width.unwrap_or(80);
        let total: usize = self.transcript.row_counts(width).iter().sum::<usize>()
            + self.transcript.live_row_count(width);
        (total, self.frame.last_viewport_rows.max(1))
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
        self.frame.last_flow_width = Some(flow.width);
        let mut counts = self.transcript.row_counts(flow.width);
        counts.push(self.transcript.live_row_count(flow.width));
        let total_rows: usize = counts.iter().sum();
        let viewport = flow.height as usize;
        let grown = total_rows.saturating_sub(self.frame.last_total_rows);
        self.scroll.on_content_grow(grown, total_rows, viewport);
        self.frame.last_total_rows = total_rows;
        self.frame.last_viewport_rows = viewport;

        // 可视窗口物化：只渲染可见行。page-flip 钉住期、跟随态与浏览态的
        // 顶行取值收敛在 ScrollState::visible_top 单点。
        let top = self.scroll.visible_top(total_rows, viewport);
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
        self.frame.last_editor_scroll_top = scroll_top;
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
        self.frame.click_targets.clear();
        if self.phase != Phase::Idle {
            // 不变量：非 Idle 时 status_spans 恒含 [stop] 段。
            #[allow(clippy::expect_used)]
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
            self.frame.click_targets.push((
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
        self.frame
            .click_targets
            .push((editor_inner, ClickTarget::Editor));

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
}

/// 按字节上限截断字符串，且不把 UTF-8 字符切半：返回截断后的前缀与
/// 是否发生截断。
fn truncate_utf8_chars(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::{CompactionState, truncate_utf8_chars};

    #[test]
    fn compaction_state_transitions_are_exclusive() {
        let mut state = CompactionState::default();
        assert!(!state.is_running());
        assert!(!state.is_queued());

        state.queue();
        assert!(!state.is_running());
        assert!(state.is_queued());

        let cancellation = state.start_if_queued().expect("queued compaction starts");
        assert!(state.is_running());
        assert!(!state.is_queued());
        assert!(!cancellation.is_cancelled());
        assert!(!state.finish());
        assert!(matches!(state, CompactionState::Idle));
    }

    #[test]
    fn finishing_a_cancelled_compaction_reports_cancellation() {
        let mut state = CompactionState::default();
        let cancellation = state.start();
        cancellation.cancel();

        assert!(state.finish());
        assert!(matches!(state, CompactionState::Idle));
    }

    #[test]
    fn truncate_utf8_chars_keeps_whole_characters_at_boundary() {
        let (text, truncated) = truncate_utf8_chars("中文paste", 4);
        assert_eq!(text, "中");
        assert!(truncated);

        let (text, truncated) = truncate_utf8_chars("ascii", 4);
        assert_eq!(text, "asci");
        assert!(truncated);

        let (text, truncated) = truncate_utf8_chars("short", 10);
        assert_eq!(text, "short");
        assert!(!truncated);
    }
}

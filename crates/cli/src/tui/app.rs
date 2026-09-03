//! TUI 应用状态：输入路由、滚动收敛、footer 合同与整帧渲染。
//!
//! [`TuiApp`] 只持有状态与事件投影；渲染在 `view`，终端 I/O 在 `tui.rs`。
//! 设置/恢复会话模态在 `modals`，会话动作在 `session_actions`，鼠标路由在
//! `mouse`，footer 合同在 `view`；跨模块共享的字段以 `pub(super)` 暴露。

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use singularity_runtime::events::{DiagnosticSeverity, ProviderAttemptStatus, TurnEvent};
use singularity_runtime::objects::TurnStatus;
use singularity_runtime::{Conversation, ThreadCatalog};

use super::commands::{Action, SlashCommand};
use super::editor::Editor;
use super::flow_select::FlowSelection;
use super::history::InputHistory;
use super::modals::{ResumeMenu, SettingsMenu};
use super::paste_burst::{CharDecision, EnterDecision, FlushOutcome, PasteBurst};
use super::scroll::ScrollState;
use super::transcript::{NoteStyle, Transcript};
use super::view::{describe_usage, highlight_piece, short_id, truncate_label};

pub(super) const SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];
const MAX_EDITOR_ROWS_CAP: u16 = 10;

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

/// 压缩期输入的注入通道（steer/followUp 双模式）：Enter 走 steer，Alt+Enter 走 followUp。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueMode {
    Steer,
    FollowUp,
}

impl QueueMode {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

/// 压缩期间暂存的一条输入，压缩结束后消费。
pub(super) struct QueuedMessage {
    pub(super) text: String,
    pub(super) mode: QueueMode,
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

/// 单帧渲染缓存：会话流宽/行数/视口度量与点击命中矩形。帧间存活，
/// 供键位滚动、内容增长检测与鼠标命中复用。
pub(super) struct FrameCache {
    pub(super) last_flow_width: Option<u16>,
    pub(super) last_total_rows: usize,
    pub(super) last_viewport_rows: usize,
    /// 编辑器最近一帧的可视滚动顶行（点击定位换算依赖）。
    pub(super) last_editor_scroll_top: usize,
    /// 会话流最近一帧的视口顶行：与流宽一起构成选区快照的有效性凭据，
    /// 任一变化即说明可见行已不是起选时的那批内容。
    pub(super) last_flow_top: usize,
    pub(super) stop_rect: Option<Rect>,
    pub(super) editor_rect: Option<Rect>,
    /// 会话流内区矩形（无边框，Paragraph 直接渲染于此）。
    pub(super) flow_rect: Option<Rect>,
    /// 最近一帧会话流可见行的纯文本（仅在有选区时物化），供松开复制取用。
    pub(super) flow_plain_rows: Vec<String>,
}

impl Default for FrameCache {
    fn default() -> Self {
        Self {
            last_flow_width: None,
            last_total_rows: 0,
            last_viewport_rows: 5,
            last_editor_scroll_top: 0,
            last_flow_top: 0,
            stop_rect: None,
            editor_rect: None,
            flow_rect: None,
            flow_plain_rows: Vec::new(),
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
    /// 二次确认退出已生效：下一次 Ctrl+C 正常退出（exit 0）。
    /// 复位规则：任何非 Ctrl+C 按键、提交输入或 turn 链结束都会清除；
    /// 按下期间提示行持续显示再次确认文案。
    pub(super) quit_armed: bool,
    pub(super) spinner_frame: usize,
    /// 当前等待对象开始等待的时刻（状态行相位计时）。
    pub(super) waiting_since: Option<std::time::Instant>,
    pub(super) turn_started_at: Option<std::time::Instant>,
    /// 状态行展示的会话累计 token 数：会话投影（`read_thread_summary`）同一
    /// 累计口径的缓存，在 turn 链结束、压缩结束与 resume 换绑时刷新；
    /// 仅存在已上报 usage 时展示。
    pub(super) session_tokens: Option<u64>,
    pub(super) frame: FrameCache,
    /// /compact 的排队、运行和取消状态。
    pub(super) compaction: CompactionState,
    /// 会话世代号：每次换绑递增。压缩完成回调携带 spawn 时的世代，
    /// 与当前世代不符即丢弃（旧会话线程不得污染新会话状态）。
    pub(super) compaction_epoch: u64,
    /// 压缩期间暂存的输入：压缩结束后首条开新回合，其余在该回合
    /// TurnStarted 时按通道注入。
    pub(super) compaction_queue: Vec<QueuedMessage>,
    /// 会话内历史（不持久化）：逐条记录提交文本，供 ↑/↓ 回溯；进入回溯前
    /// 的草稿由 [`InputHistory`] 持有，退出回溯且未编辑时恢复。换绑会话时
    /// 整体重建。
    pub(super) history: InputHistory,
    /// 非括号粘贴的按键突发检测：缺省括号粘贴的终端把粘贴送达为高速按键流，
    /// 在此拼回单个粘贴。换绑会话时整体重建（残留突发不得污染新会话）。
    pub(super) burst: PasteBurst,
    /// 会话流拖选（视口坐标快照）：松开即复制，视口顶行或流宽变化即失效。
    pub(super) flow_selection: Option<FlowSelection>,
    /// 系统剪贴板：首次复制时惰性建立并常驻（Windows 下 arboard 每次操作
    /// 自行开关剪贴板，单线程调用即其推荐用法）。
    pub(super) clipboard: Option<arboard::Clipboard>,
}

impl TuiApp {
    pub fn new(conversation: Arc<Conversation>) -> Self {
        let thread_catalog = ThreadCatalog::new(conversation.runner_handle().as_ref());
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
            quit_armed: false,
            spinner_frame: 0,
            waiting_since: None,
            turn_started_at: None,
            session_tokens: None,
            frame: FrameCache::default(),
            compaction: CompactionState::default(),
            compaction_epoch: 0,
            compaction_queue: Vec::new(),
            history: InputHistory::default(),
            burst: PasteBurst::default(),
            flow_selection: None,
            clipboard: None,
        }
    }

    pub fn conversation_handle(&self) -> Arc<Conversation> {
        Arc::clone(&self.conversation)
    }

    /// 当前换绑 thread 的 id。
    pub(super) fn current_thread_id(&self) -> String {
        self.conversation.thread().thread_id
    }

    /// 从会话投影刷新状态行的累计用量缓存（唯一事实源）。
    pub(super) fn refresh_session_tokens(&mut self) {
        self.session_tokens = self
            .thread_catalog
            .read_thread_summary(&self.current_thread_id())
            .ok()
            .and_then(|summary| (summary.total_tokens > 0).then_some(summary.total_tokens));
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
                // 注入窗口已开：把压缩队列的剩余消息按通道送达本回合。
                self.inject_compaction_queue_rest();
            }
            TurnEvent::AssistantDelta { delta, .. } => {
                // delta 恒属于本轮 assistant 条目：runner 以固定 item id
                // `{turn_id}_assistant` 发布，工具 item id 不可能混入。
                self.transcript.assistant_delta(delta);
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
            TurnEvent::ItemCompleted { item, .. } | TurnEvent::ItemFailed { item, .. } => {
                if self.transcript.is_tool_item(&item.item_id) {
                    self.transcript.tool_terminal(
                        &item.item_id,
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
                ..
            } => {
                self.transcript
                    .tool_end(tool_call_id, result.text_content(), result.is_error);
                self.set_waiting(WaitingTarget::Model);
            }
            TurnEvent::Diagnostic {
                severity,
                code,
                message,
                ..
            } => {
                let style = match severity {
                    DiagnosticSeverity::Error => NoteStyle::Error,
                    DiagnosticSeverity::Warning => NoteStyle::Warning,
                    DiagnosticSeverity::Info => NoteStyle::Dim,
                };
                self.transcript
                    .push_note(format!("⚠ [{severity}] {code}: {message}"), style);
            }
            TurnEvent::AssistantThinking { text, .. } => {
                self.transcript.push_thinking(text);
            }
            TurnEvent::TurnCompleted { turn } => {
                // 终局状态只来自事件（唯一投影）：interrupted 终态携带
                // interrupted 状态，UI 不再从链回执复制第二份终态机。
                match turn.status {
                    TurnStatus::Interrupted => {
                        self.transcript.push_note(
                            format!("⚠ interrupted ({})", describe_usage(turn)),
                            NoteStyle::Warning,
                        );
                    }
                    _ => {
                        self.transcript.push_note(
                            format!("✔ completed ({})", describe_usage(turn)),
                            NoteStyle::Dim,
                        );
                    }
                }
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

    /// 整个 run_turn 调用结束（含其后续队列执行完毕）。
    /// turn 链终态回调：可信终态（含失败与中断）的展示全部来自事件投影，
    /// 回执只复位运行相位；无可信终态的链中止（准备/终态化/占用失败）
    /// 才由回执携带报告文本。存在排队压缩时武装并返回 `Action::Compact`
    /// 由事件循环 spawn 后台压缩线程。
    pub fn on_chain_finished(&mut self, result: &Result<(), String>) -> Action {
        self.phase = Phase::Idle;
        self.set_waiting(WaitingTarget::None);
        self.quit_armed = false;
        self.turn_started_at = None;
        self.refresh_session_tokens();
        if let Err(message) = result {
            self.transcript
                .push_note(format!("✖ {message}"), NoteStyle::Error);
        }
        // 排队压缩在 turn 终态后自动启动（复用同一压缩路径与取消令牌）。
        if let Some(cancellation) = self.compaction.start_if_queued() {
            self.transcript
                .push_note("compacting context…", NoteStyle::Dim);
            return Action::Compact(cancellation, self.compaction_epoch);
        }
        Action::Continue
    }

    /// 中断时未交付的转向输入退还编辑器：合并进
    /// 当前编辑器文本，便于用户编辑后重新提交。取展开文本，
    /// 占位标签不得漏进重提内容。
    pub fn return_undelivered(&mut self, inputs: Vec<String>) {
        if inputs.is_empty() {
            return;
        }
        let mut draft = self.editor.take_expanded();
        for text in inputs {
            if !draft.is_empty() {
                draft.push('\n');
            }
            draft.push_str(&text);
        }
        self.editor.set_text(&draft);
        self.transcript
            .push_note("interrupted input returned to the editor", NoteStyle::Dim);
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
            return Action::Exit;
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

    /// 带显式时钟的键盘路由：生产循环传入事件到达时刻；测试传入伪造时刻
    /// 以确定性覆盖突发窗口。纯 Enter（无 Shift/Alt）在突发上下文中改走
    /// 换行而非提交；其余所有按键先强制落定突发暂存再处理。
    pub(super) fn handle_key_at(
        &mut self,
        key: crossterm::event::KeyEvent,
        now: std::time::Instant,
    ) -> Action {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Action::Continue;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let plain_enter = key.code == KeyCode::Enter && !shift && !alt;
        // 纯文本字符走突发时序检测（不得在此冲刷，否则 hold 永不成立）；
        // 纯 Enter 由突发咨询接管；其余所有按键先强制落定暂存再处理。
        let plain_char = matches!(key.code, KeyCode::Char(_)) && !ctrl && !alt;
        if !plain_enter && !plain_char {
            self.flush_burst_forced(now);
        }

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

        match key.code {
            KeyCode::Char('t') if ctrl => self.transcript.toggle_thinking(),
            KeyCode::Char('j') if ctrl => {
                self.exit_history_after_edit();
                self.editor.insert_newline();
            }
            KeyCode::Char('o') if ctrl || alt => {
                self.transcript.toggle_latest_tool_expansion();
            }
            KeyCode::Esc => {
                // Esc 只管中断：闲时回到跟随（滚屏中），忙时中断 turn；
                // 输入清空走鼠标拖选+删除，不再由 Esc 猜。
                if self.phase == Phase::Idle {
                    let (total, viewport) = self.flow_metrics();
                    if !self.scroll.is_following() {
                        self.scroll.jump_to_bottom(total, viewport);
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
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => return Action::Exit,
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
                // 出队优先压缩队列（恢复全部暂存输入）；
                // 压缩队列为空时维持既有 followUp 逐条撤回。
                if !self.compaction_queue.is_empty() {
                    self.dequeue_compaction_queue();
                } else if let Some(text) = self.conversation.withdraw_follow_up() {
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
            KeyCode::Enter if shift => {
                self.exit_history_after_edit();
                self.editor.insert_newline();
            }
            KeyCode::Enter if alt => {
                if self.compaction.is_running() {
                    return self.queue_during_compaction(QueueMode::FollowUp);
                }
                if self.phase != Phase::Idle {
                    self.submit_follow_up();
                }
            }
            // 纯 Enter：突发上下文（非括号粘贴的高速按键流）中改为换行，
            // 不得提交；其余走原提交路径。
            KeyCode::Enter => match self.burst.on_enter(now) {
                EnterDecision::Buffered => {}
                EnterDecision::LocalNewline => {
                    self.exit_history_after_edit();
                    self.editor.insert_newline();
                }
                EnterDecision::Submit => return self.submit_input(),
            },
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
            KeyCode::Char(ch) if !ctrl && !alt => match self.burst.on_char(ch, now) {
                CharDecision::Held | CharDecision::Buffered => {}
                // 直通/姗姗来迟的落定字：按打字立即插入。
                CharDecision::Typed(ch) => {
                    self.exit_history_after_edit();
                    self.editor.insert_char(ch);
                }
            },
            _ => {}
        }
        Action::Continue
    }

    /// 显式粘贴（bracketed paste 事件）与突发落定：CRLF/CR 归一后进编辑器
    /// 统一入口。长短路由与会话合并（拆分投递并块）由编辑器负责。
    pub fn handle_paste(&mut self, text: String, now: std::time::Instant) {
        self.exit_history_after_edit();
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.editor.insert_paste(text, now);
    }

    /// 同一批次到达的纯文本按键突发（含 Enter）：缺省括号粘贴的终端分块
    /// 投递时，按单次粘贴整体应用，Enter 不得提交。判据保守——整批全是
    /// Press 按键、且全是无修饰纯文本/Enter、含 Enter、总数≥3：人类在单轮
    /// 事件排空内凑不齐（30Hz 按键重复也远不够），误伤不了打字与连击
    /// Enter 提交；小批量仍走时序检测。命中返回 true（调用方跳过逐键处理）。
    pub(super) fn apply_key_burst(
        &mut self,
        batch: &[crossterm::event::Event],
        now: std::time::Instant,
    ) -> bool {
        use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
        if batch.len() < 3 {
            return false;
        }
        let mut text = String::new();
        let mut enters = 0usize;
        for event in batch {
            let Event::Key(key) = event else {
                return false;
            };
            if key.kind != KeyEventKind::Press {
                return false;
            }
            match key.code {
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    text.push(ch);
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    text.push('\n');
                    enters += 1;
                }
                _ => return false,
            }
        }
        if enters == 0 {
            return false;
        }
        self.reset_quit_confirm();
        self.handle_paste(text, now);
        true
    }

    /// 突发暂存的强制落定：纯 Enter 与纯文本字符之外的按键处理前调用，先应用再处理
    /// 该键（否则暂存滞留，后续光标移动错位、Ctrl+C 清空漏字）。
    fn flush_burst_forced(&mut self, now: std::time::Instant) {
        let outcome = self.burst.flush_forced();
        self.apply_burst_outcome(outcome, now);
    }

    /// 到期冲刷（事件循环每轮调用）：有落定内容时返回 true（需重绘）。
    pub(super) fn flush_burst_if_due(&mut self, now: std::time::Instant) -> bool {
        let outcome = self.burst.flush_if_due(now);
        let applied = !matches!(outcome, FlushOutcome::None);
        self.apply_burst_outcome(outcome, now);
        applied
    }

    pub(super) fn apply_burst_outcome(&mut self, outcome: FlushOutcome, now: std::time::Instant) {
        match outcome {
            FlushOutcome::Paste(pasted) => self.handle_paste(pasted, now),
            FlushOutcome::Typed(ch) => {
                self.exit_history_after_edit();
                self.editor.insert_char(ch);
            }
            FlushOutcome::None => {}
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
            // 草稿存展开文本：占位标签不得进历史，恢复后内容不丢。
            if let Some(text) = self.history.enter(self.editor.expanded_text()) {
                self.editor.set_text(text);
            }
            return true;
        }
        false
    }

    /// ↓ 处理：回溯中下一条，到最新后退出回溯并恢复草稿（回溯期间发生过
    /// 编辑时草稿已被丢弃，编辑器保持当前内容）；否则返回 false 由调用方
    /// 执行 move_down。
    fn handle_history_down(&mut self) -> bool {
        if !self.history.is_navigating() {
            return false;
        }
        if let Some(text) = self.history.down() {
            self.editor.set_text(text);
            return true;
        }
        // 已到最新并退出回溯。
        if let Some(draft) = self.history.take_draft() {
            self.editor.set_text(&draft);
        }
        true
    }

    /// 回溯中发生编辑（插入/删除/粘贴/Enter 提交）：退出回溯，保留
    /// 当前内容（草稿丢弃）。
    pub(super) fn exit_history_after_edit(&mut self) {
        if self.history.is_navigating() {
            self.history.exit_keeping();
        }
    }

    /// 记录一条提交到历史，复位回溯指针与草稿。
    pub(super) fn record_history(&mut self, text: &str) {
        self.history.record(text);
    }

    fn submit_input(&mut self) -> Action {
        // 压缩持有一致性写窗口：文本输入排队、压缩结束后消费，斜杠命令
        // 立即执行。
        if self.compaction.is_running() {
            return self.queue_during_compaction(QueueMode::Steer);
        }
        // 提交即取走全部输入，编辑器为空后进入下一次编辑会话。
        // 粘贴块在此展开为全文（展示标签只活在编辑器内）。
        self.exit_history_after_edit();
        let raw = self.editor.take_expanded();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return Action::Continue;
        }
        // 斜杠命令精确匹配：命中的执行命令，其余一切（含 / 开头）走消息路径。
        if let Some(command) = SlashCommand::parse(&text) {
            return self.execute_command(command);
        }
        match self.phase {
            Phase::Idle => {
                self.record_history(&text);
                self.begin_turn(text)
            }
            Phase::Running => {
                let accepted = self.conversation.steer(text.clone());
                if accepted {
                    // 接受才记历史，与 followUp 路径统一。
                    self.record_history(&text);
                }
                self.note_injection("steer", accepted, &text);
                // steer 注入后回到最新内容。
                let (total, viewport) = self.flow_metrics();
                self.scroll.jump_to_bottom(total, viewport);
                if !accepted {
                    // 空闲竞态：直接开新回合。
                    return self.begin_turn(text);
                }
                Action::Continue
            }
        }
    }

    /// 开新回合的统一序列：视口回底跟随、进入运行相位、把输入
    /// 物化进会话流，并返回交事件循环 spawn 的 [`Action::Submit`]。
    pub(super) fn begin_turn(&mut self, text: String) -> Action {
        let (total, viewport) = self.flow_metrics();
        self.scroll.jump_to_bottom(total, viewport);
        self.phase = Phase::Running;
        self.set_waiting(WaitingTarget::Model);
        self.transcript.push_user(text.clone());
        Action::Submit(text)
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
        // 编辑器内容每帧折行一次：高度与渲染共用同一份可视片段。
        let editor_pieces = self.editor.wrapped_pieces(inner_width);
        let editor_rows = editor_pieces.len().clamp(1, max_editor_rows as usize) as u16 + 2;
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
        let width_changed = self
            .frame
            .last_flow_width
            .is_some_and(|previous| previous != flow.width);
        self.frame.last_flow_width = Some(flow.width);
        let mut counts = self.transcript.row_counts(flow.width);
        counts.push(self.transcript.live_row_count(flow.width));
        let total_rows: usize = counts.iter().sum();
        let viewport = flow.height as usize;
        let grown = total_rows.saturating_sub(self.frame.last_total_rows);
        self.scroll.on_content_grow(grown, total_rows, viewport);
        self.frame.last_total_rows = total_rows;
        self.frame.last_viewport_rows = viewport;

        // 可视窗口物化：只渲染可见行。跟随态与浏览态的
        // 顶行取值收敛在 ScrollState::visible_top 单点；每条目物化一次后
        // 切片，`counts` 末尾的 live 伪条目与定稿条目同一条路径。
        let top = self.scroll.visible_top(total_rows, viewport);
        // 选区快照失效：顶行一变，或流宽一变，可见行已不是起选时那批内容。
        if self.flow_selection.is_some() && (self.frame.last_flow_top != top || width_changed) {
            self.flow_selection = None;
            self.frame.flow_plain_rows.clear();
        }
        self.frame.last_flow_top = top;
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
        if total_rows > 0 && viewport > 0 {
            let spinner = SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
            let spinner = if self.phase == Phase::Idle || self.waiting == WaitingTarget::None {
                ' '
            } else {
                spinner
            };
            let mut offset = 0usize;
            for (item_index, rows) in counts.iter().enumerate() {
                let end = offset + rows;
                if end > top && offset < top + viewport {
                    let item_rows = self
                        .transcript
                        .render_item_rows(item_index, flow.width, spinner);
                    let from = top.saturating_sub(offset);
                    let to = (top + viewport).saturating_sub(offset).min(item_rows.len());
                    lines.extend(
                        item_rows
                            .into_iter()
                            .skip(from)
                            .take(to.saturating_sub(from)),
                    );
                }
                offset = end;
                if offset >= top + viewport {
                    break;
                }
            }
        }
        while (lines.len() as u16) < flow.height {
            lines.push(Line::from(Span::raw(String::new())));
        }
        // 有选区时才物化纯文本行：松开复制读的就是这批可见内容。
        if self.flow_selection.is_some() {
            self.frame.flow_plain_rows = lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect();
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), flow);
        self.frame.flow_rect = Some(flow);
        // 选区反白：逐行按显示列区间叠加 REVERSED，保留原有配色。
        if let Some(selection) = self.flow_selection {
            let style = Style::new().add_modifier(Modifier::REVERSED);
            for row in 0..flow.height as usize {
                let Some((from, to)) = selection.cols_on(row) else {
                    continue;
                };
                let (from, to) = (from.min(flow.width as usize), to.min(flow.width as usize));
                if from < to {
                    frame.buffer_mut().set_style(
                        Rect::new(
                            flow.x.saturating_add(from as u16),
                            flow.y.saturating_add(row as u16),
                            (to - from) as u16,
                            1,
                        ),
                        style,
                    );
                }
            }
        }

        // 编辑器：高度随内容增长（钳制上限），光标始终可见；只物化可见
        // 窗口的行，大粘贴的不可见行不进 `Line` 分配。
        let editor_inner_w = editor_area.width.saturating_sub(2).max(1);
        let inner_h = editor_rows.saturating_sub(2) as usize;
        let (visual_row, visual_col) = self.editor.cursor_visual(editor_inner_w);
        // 滚轮覆盖优先，否则跟随光标。
        let scroll_top = self.editor.effective_scroll_top(visual_row, inner_h);
        self.frame.last_editor_scroll_top = scroll_top;
        let editor_lines: Vec<Line<'static>> = {
            // 选中反白：与上面同一宽度（命中同一份折行备忘，不重复折行），
            // 按全局可视行对号入座；无选择时退化为原来的整行 raw。
            let selected = self.editor.selection_spans(editor_inner_w);
            editor_pieces
                .into_iter()
                .enumerate()
                .skip(scroll_top)
                .take(inner_h)
                .map(|(index, piece)| {
                    let ranges: Vec<(usize, usize)> = selected
                        .iter()
                        .filter(|span| span.0 == index)
                        .map(|&(_, from, to)| (from, to))
                        .collect();
                    highlight_piece(piece, &ranges)
                })
                .collect()
        };
        frame.render_widget(
            Paragraph::new(editor_lines)
                .block(Block::default().borders(Borders::ALL).title("input")),
            editor_area,
        );

        // 状态行 + 提示行。
        let (status_spans, hint_spans, stop_width) =
            self.footer_spans(total_rows, viewport, status_area.width);
        frame.render_widget(Paragraph::new(vec![Line::from(status_spans)]), status_area);
        frame.render_widget(Paragraph::new(vec![Line::from(hint_spans)]), hint_area);

        // 点击命中缓存：登记本帧可点击矩形。[stop] 矩形由状态行收尾合同
        // 给出的列宽与状态行右缘直接推出；编辑器内区不含边框。
        self.frame.stop_rect = stop_width.map(|width| {
            Rect::new(
                status_area.right().saturating_sub(width),
                status_area.y,
                width,
                1,
            )
        });
        self.frame.editor_rect = Some(Rect {
            x: editor_area.x.saturating_add(1),
            y: editor_area.y.saturating_add(1),
            width: editor_area.width.saturating_sub(2).max(1),
            height: editor_area.height.saturating_sub(2).max(1),
        });

        if let Some(menu) = &self.settings {
            Self::render_settings(frame, menu);
        } else if let Some(menu) = &self.resume {
            Self::render_resume(frame, menu);
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

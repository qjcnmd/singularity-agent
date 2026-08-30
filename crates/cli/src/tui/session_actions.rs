//! 会话动作：斜杠命令执行、会话换绑与上下文压缩的异步编排。
//!
//! 换绑统一入口 [`TuiApp::rebind_conversation`] 消除 resume/new 双份
//! conversation/thread/transcript/scroll 重置逻辑；/compact 不再同步阻塞
//! 事件循环，而是返回 [`Action::Compact`] 由事件循环 spawn 后台线程执行，
//! Esc 通过外部 [`CancellationToken`] 取消本次压缩。

use std::sync::Arc;

use singularity_runtime::{CompactionOutcome, Conversation, HistoryItem, Thread, TurnRunner};

use super::app::{CompactionState, Phase, QueueMode, QueuedMessage, TuiApp, WaitingTarget};
use super::commands::{Action, SlashCommand};
use super::modals::{ResumeMenu, SettingsMenu};
use super::scroll::ScrollState;
use super::transcript::{NoteStyle, Transcript};
use super::view::{short_id, truncate_label};

/// /resume 重放的轮次上限；与 thread/read 协议单页上限一致。
const REPLAY_TURN_LIMIT: usize = 200;

impl TuiApp {
    /// 会话态整体重置：换绑新会话时归位全部运行相位与临时状态，
    /// 防新增字段在换绑后残留旧会话的事实。进行中的压缩同步取消，
    /// 世代递增使旧压缩线程的迟到回调整体作废。
    fn reset_session_state(&mut self) {
        if let CompactionState::Running(token) = &self.compaction {
            token.cancel();
        }
        self.compaction_epoch = self.compaction_epoch.wrapping_add(1);
        self.phase = Phase::Idle;
        self.waiting = WaitingTarget::None;
        self.waiting_since = None;
        self.turn_started_at = None;
        self.compaction = CompactionState::default();
        self.compaction_queue.clear();
        self.settings = None;
        self.resume = None;
    }

    /// 会话换绑统一入口：替换 conversation、thread_id、transcript、scroll
    /// 与 session_tokens，并整体重置会话态，消除 resume/new 双份换绑逻辑。
    fn rebind_conversation(
        &mut self,
        runner: Arc<TurnRunner>,
        thread: Thread,
        session_tokens: Option<u64>,
    ) {
        let thread_id = thread.thread_id.clone();
        self.conversation = Conversation::new(runner, thread);
        self.thread_id = thread_id;
        self.transcript = Transcript::new();
        self.scroll = ScrollState::default();
        self.session_tokens = session_tokens;
        self.reset_session_state();
    }

    /// 换绑到已持久化的会话；失败时只记 note，状态不变。
    pub(super) fn resume_thread(&mut self, thread_id: &str) {
        let runner = self.conversation.runner_handle();
        match self.thread_catalog.resume_thread(thread_id) {
            Ok(thread) => {
                let session_tokens = self
                    .thread_catalog
                    .read_thread_summary(&thread.thread_id)
                    .ok()
                    .and_then(|summary| (summary.total_tokens > 0).then_some(summary.total_tokens));
                let thread_id = thread.thread_id.clone();
                self.rebind_conversation(runner, thread, session_tokens);
                self.replay_history(&thread_id);
                self.transcript.push_note(
                    format!("resumed thread {}", short_id(&thread_id)),
                    NoteStyle::Accent,
                );
            }
            Err(error) => self
                .transcript
                .push_note(format!("resume failed: {error}"), NoteStyle::Error),
        }
    }

    /// /resume 后按 `paged_read` 重放历史：物化 user/assistant/thinking/tool
    /// 条目为会话流。重放只发生在 resume 换绑路径，
    /// /new 与首启保持空流；读取失败时静默跳过（note 仍提示 resume 成功）。
    fn replay_history(&mut self, thread_id: &str) {
        let Ok(page) = self
            .thread_catalog
            .paged_read(thread_id, REPLAY_TURN_LIMIT, None)
        else {
            return;
        };
        let mut unresulted_calls: Vec<String> = Vec::new();
        for turn in &page.turns {
            for item in &turn.items {
                match item {
                    HistoryItem::Message { role, text, .. } => match role.as_str() {
                        "user" => self.transcript.push_user(text.clone()),
                        _ => self.transcript.push_note(text.clone(), NoteStyle::Info),
                    },
                    HistoryItem::Thinking { text, .. } => {
                        self.transcript.push_thinking(text.clone());
                    }
                    HistoryItem::ToolCall { id, name, args } => {
                        unresulted_calls.push(id.clone());
                        self.transcript.tool_start(id, name, args);
                    }
                    HistoryItem::ToolResult {
                        id,
                        output,
                        is_error,
                    } => {
                        unresulted_calls.retain(|call| call != id);
                        self.transcript.tool_end(id, output, *is_error);
                    }
                    _ => {}
                }
            }
        }
        // 崩溃遗留的孤立工具调用（无 ToolResult 收尾）定型为稳定记录，
        // 不停留在运行中渲染。
        for call_id in &unresulted_calls {
            self.transcript.tool_terminal(call_id, false);
        }
    }

    /// 斜杠命令分发。`/compact` 不在此处阻塞执行：置位压缩状态并返回
    /// [`Action::Compact`]，由事件循环 spawn 后台线程真正执行。
    /// `/name` 打开单行命名输入弹窗，确认后走 `thread_catalog.rename` 路径。
    pub(super) fn execute_command(&mut self, command: SlashCommand) -> Action {
        match command {
            SlashCommand::Model => {
                let current = self
                    .conversation
                    .thread()
                    .ok()
                    .and_then(|thread| thread.model);
                self.settings = Some(SettingsMenu::open_field(current.as_deref(), 1));
            }
            SlashCommand::Settings => {
                let current = self
                    .conversation
                    .thread()
                    .ok()
                    .and_then(|thread| thread.model);
                self.settings = Some(SettingsMenu::open(current.as_deref()));
            }
            SlashCommand::Resume => {
                // 换绑受相位门禁：菜单期间旧 turn 的终态事件会错乱投影进
                // 新会话流；TUI 相位与 runtime 活动窗口双重检查，运行中
                // 先结束当前回合。
                if self.phase != Phase::Idle || self.conversation.has_active_turn() {
                    self.transcript.push_note(
                        "turn in progress; press Esc to end it before switching sessions",
                        NoteStyle::Warning,
                    );
                    return Action::Continue;
                }
                match self.thread_catalog.list_threads() {
                    Ok(threads) if !threads.is_empty() => {
                        self.resume = Some(ResumeMenu::new(threads));
                    }
                    Ok(_) => self
                        .transcript
                        .push_note("no saved sessions", NoteStyle::Dim),
                    Err(error) => self.transcript.push_note(error, NoteStyle::Error),
                }
            }
            SlashCommand::New => {
                if self.phase != Phase::Idle || self.conversation.has_active_turn() {
                    self.transcript.push_note(
                        "turn in progress; press Esc to end it before switching sessions",
                        NoteStyle::Warning,
                    );
                    return Action::Continue;
                }
                let runner = self.conversation.runner_handle();
                let current = self.conversation.thread().ok();
                let cwd = current
                    .as_ref()
                    .map(|thread| thread.cwd.clone())
                    .unwrap_or_default();
                let model = current.and_then(|thread| thread.model);
                match self.thread_catalog.create_thread(&cwd, model) {
                    Ok(thread) => {
                        let thread_id = thread.thread_id.clone();
                        self.rebind_conversation(runner, thread, None);
                        self.transcript.push_note(
                            format!("new thread {}", short_id(&thread_id)),
                            NoteStyle::Accent,
                        );
                    }
                    Err(error) => self.transcript.push_note(error, NoteStyle::Error),
                }
            }
            SlashCommand::Session => {
                match self.thread_catalog.read_thread_summary(&self.thread_id) {
                    Ok(summary) => self.transcript.push_note(
                        format!(
                            "session {} · {} turns · {} tokens",
                            summary.thread_id, summary.turn_count, summary.total_tokens
                        ),
                        NoteStyle::Accent,
                    ),
                    Err(_) => self
                        .transcript
                        .push_note("session facts unavailable", NoteStyle::Warning),
                }
            }
            SlashCommand::Compact => {
                if self.compaction.is_running() {
                    self.transcript
                        .push_note("compaction already in progress", NoteStyle::Dim);
                } else if self.compaction.is_queued() {
                    self.transcript
                        .push_note("compaction already queued", NoteStyle::Dim);
                } else if self.conversation.has_active_turn() {
                    // 活动 turn 期间排队，turn 到达终态后由
                    // `on_chain_finished` 自动启动压缩。
                    self.compaction.queue();
                    self.transcript.push_note(
                        "compaction queued; will run when the turn finishes",
                        NoteStyle::Dim,
                    );
                } else {
                    let cancellation = self.compaction.start();
                    self.transcript
                        .push_note("compacting context…", NoteStyle::Dim);
                    return Action::Compact(cancellation, self.compaction_epoch);
                }
            }
            SlashCommand::Name => {
                // 打开单行命名输入弹窗（复用 SettingsMenu 的字段编辑模式）。
                let current_name = self
                    .thread_catalog
                    .read_thread_summary(&self.thread_id)
                    .ok()
                    .and_then(|summary| summary.title)
                    .unwrap_or_default();
                self.settings = Some(SettingsMenu::open_name(Some(&current_name)));
            }
        }
        Action::Continue
    }

    /// 队列注入 followUp（运行中 Alt+Enter）。
    pub(super) fn submit_follow_up(&mut self) {
        let text = self.editor.take().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.exit_history_after_edit();
        let accepted = self.conversation.submit_follow_up(text.clone());
        self.note_injection("followUp", accepted, &text);
        if accepted {
            self.record_history(&text);
        }
    }

    /// 压缩期输入：斜杠命令立即执行，
    /// 文本按通道入队——Enter 走 steer、Alt+Enter 走 followUp，压缩结束
    /// 后由 [`TuiApp::on_compact_finished`] 消费。
    pub(super) fn queue_during_compaction(&mut self, mode: QueueMode) -> Action {
        self.paste_burst.clear_after_explicit_paste();
        self.exit_history_after_edit();
        let text = self.editor.take().trim().to_string();
        if text.is_empty() {
            return Action::Continue;
        }
        if let Some(command) = SlashCommand::parse(&text) {
            self.record_history(&text);
            return self.execute_command(command);
        }
        self.record_history(&text);
        self.transcript.push_note(
            format!(
                "queued {} for after compaction: {}",
                mode.label(),
                truncate_label(&text, 40)
            ),
            NoteStyle::Dim,
        );
        self.compaction_queue.push(QueuedMessage { text, mode });
        Action::Continue
    }

    /// 出队：压缩队列整体倒回编辑器供编辑，队列文本与当前草稿以空行拼接。
    pub(super) fn dequeue_compaction_queue(&mut self) {
        let queued = std::mem::take(&mut self.compaction_queue);
        let count = queued.len();
        let current = self.editor.text();
        let combined = queued
            .iter()
            .map(|msg| msg.text.as_str())
            .chain(std::iter::once(current.as_str()))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.editor.set_text(&combined);
        self.transcript.push_note(
            format!("restored {count} queued message(s) to editor"),
            NoteStyle::Accent,
        );
    }

    /// 压缩队列的注入段：回合启动（注入窗口已开）后把剩余消息按通道
    /// 送达；注入失败的退回队列，等待下一次回合或 Alt+Up 取回，不丢输入。
    pub(super) fn inject_compaction_queue_rest(&mut self) {
        for msg in std::mem::take(&mut self.compaction_queue) {
            let accepted = match msg.mode {
                QueueMode::Steer => self.conversation.steer(msg.text.clone()),
                QueueMode::FollowUp => self.conversation.submit_follow_up(msg.text.clone()),
            };
            self.note_injection(msg.mode.label(), accepted, &msg.text);
            if !accepted {
                self.compaction_queue.push(msg);
            }
        }
    }

    /// 注入结果回显：接受时以用户消息样式本地回显全文；被拒时保留
    /// 「已注入/被拒」提示（文案与内容截断保持一致）。
    pub(super) fn note_injection(&mut self, kind: &str, accepted: bool, text: &str) {
        if accepted {
            self.transcript.push_user(text.to_string());
            return;
        }
        let label = truncate_label(text, 40);
        self.transcript.push_note(
            format!("↳ {kind} rejected (turn closed): {label}"),
            NoteStyle::Accent,
        );
    }

    // -- 压缩异步编排 --------------------------------------------------------

    /// 压缩进行中取消本次压缩：触发令牌取消；收尾文案由
    /// [`TuiApp::on_compact_finished`] 统一给出。
    pub(super) fn cancel_compact(&mut self) {
        if let CompactionState::Running(token) = &self.compaction {
            token.cancel();
        }
    }

    /// 压缩线程完成回调：复位压缩状态、把结果投影为会话流 note，并消费
    /// 压缩队列——首条按普通提交开新回合，其余待该回合 TurnStarted 时注入。
    /// 回调携带 spawn 时的会话世代；世代不符说明会话已换绑，回调整体丢弃。
    pub(super) fn on_compact_finished(
        &mut self,
        epoch: u64,
        result: Result<CompactionOutcome, String>,
    ) -> Action {
        if epoch != self.compaction_epoch {
            return Action::Continue;
        }
        let cancelled = self.compaction.finish();
        match result {
            Ok(CompactionOutcome::Compacted { tokens_before, .. }) => self.transcript.push_note(
                format!("context compacted from {tokens_before} estimated tokens"),
                NoteStyle::Accent,
            ),
            Ok(CompactionOutcome::NotNeeded) => self
                .transcript
                .push_note("nothing to compact", NoteStyle::Dim),
            Err(_) if cancelled => self
                .transcript
                .push_note("compaction cancelled", NoteStyle::Warning),
            Err(error) => self
                .transcript
                .push_note(format!("compaction failed: {error}"), NoteStyle::Error),
        }
        // 队列与压缩结果无关地消费（取消/失败同样送达）。
        if self.compaction_queue.is_empty() {
            return Action::Continue;
        }
        let first = self.compaction_queue.remove(0);
        let (total, _) = self.flow_metrics();
        self.scroll.pin_new_content_at(total);
        self.phase = Phase::Running;
        self.set_waiting(WaitingTarget::Model);
        self.transcript.push_user(first.text.clone());
        Action::Submit(first.text)
    }
}

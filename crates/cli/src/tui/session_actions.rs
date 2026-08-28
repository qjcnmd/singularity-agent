//! 会话动作：斜杠命令执行、会话换绑与上下文压缩的异步编排。
//!
//! 换绑统一入口 [`TuiApp::rebind_conversation`] 消除 resume/new 双份
//! conversation/thread/transcript/scroll 重置逻辑；/compact 不再同步阻塞
//! 事件循环，而是返回 [`Action::Compact`] 由事件循环 spawn 后台线程执行，
//! Esc 通过外部 [`CancellationToken`] 取消本次压缩。

use std::sync::Arc;

use singularity_runtime::{CompactionOutcome, Conversation, Thread, TurnRunner};

use super::app::{CompactionState, Phase, TuiApp, WaitingTarget};
use super::commands::{Action, SlashCommand};
use super::modals::{ResumeMenu, SettingsMenu};
use super::scroll::ScrollState;
use super::transcript::{NoteStyle, Transcript};
use super::view::{short_id, truncate_label};

impl TuiApp {
    /// 会话态整体重置：换绑新会话时归位全部运行相位与临时状态，
    /// 防新增字段在换绑后残留旧会话的事实。
    fn reset_session_state(&mut self) {
        self.phase = Phase::Idle;
        self.waiting = WaitingTarget::None;
        self.waiting_since = None;
        self.turn_started_at = None;
        self.compaction = CompactionState::default();
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
                    .list_threads()
                    .ok()
                    .and_then(|threads| {
                        threads
                            .into_iter()
                            .find(|summary| summary.thread_id == thread.thread_id)
                    })
                    .and_then(|summary| (summary.total_tokens > 0).then_some(summary.total_tokens));
                let thread_id = thread.thread_id.clone();
                self.rebind_conversation(runner, thread, session_tokens);
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

    /// 斜杠命令分发。`/compact` 不在此处阻塞执行：置位压缩状态并返回
    /// [`Action::Compact`]，由事件循环 spawn 后台线程真正执行。
    pub(super) fn execute_command(&mut self, text: &str) -> Action {
        let Some((command, argument)) = SlashCommand::parse(text) else {
            self.transcript
                .push_note(format!("unknown command: {text}"), NoteStyle::Warning);
            return Action::Continue;
        };
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
                // 新会话流，运行中先结束当前回合。
                if self.phase != Phase::Idle {
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
                if self.phase != Phase::Idle {
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
                let summary = self.thread_catalog.list_threads().ok().and_then(|threads| {
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
            SlashCommand::Compact => {
                if self.compaction.is_running() {
                    self.transcript
                        .push_note("compaction already in progress", NoteStyle::Dim);
                } else if self.compaction.is_queued() {
                    self.transcript
                        .push_note("compaction already queued", NoteStyle::Dim);
                } else if self.conversation.has_active_turn() {
                    // 活动 turn 期间排队，turn 到达终态后由
                    // `on_chain_finished` 自动启动压缩（与 pi 的
                    // "waiting until idle" 语义一致）。
                    self.compaction.queue();
                    self.transcript.push_note(
                        "compaction queued; will run when the turn finishes",
                        NoteStyle::Dim,
                    );
                } else {
                    let cancellation = self.compaction.start();
                    self.transcript
                        .push_note("compacting context…", NoteStyle::Dim);
                    return Action::Compact(cancellation);
                }
            }
            SlashCommand::Name if !argument.trim().is_empty() => {
                let rename = if self.conversation.has_active_turn() {
                    Err(singularity_runtime::ConversationError::TurnAlreadyActive.to_string())
                } else {
                    self.thread_catalog.rename(&self.thread_id, argument.trim())
                };
                match rename {
                    Ok(()) => self.transcript.push_note(
                        format!("session named {}", argument.trim()),
                        NoteStyle::Accent,
                    ),
                    Err(error) => self.transcript.push_note(error, NoteStyle::Error),
                }
            }
            SlashCommand::Name => self
                .transcript
                .push_note("usage: /name <session name>", NoteStyle::Warning),
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

    /// 向会话流注入一条「已注入/被拒」提示，文案与内容截断保持一致。
    pub(super) fn note_injection(&mut self, kind: &str, accepted: bool, text: &str) {
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

    // -- 压缩异步编排 --------------------------------------------------------

    /// 压缩进行中取消本次压缩：触发令牌取消；收尾文案由
    /// [`TuiApp::on_compact_finished`] 统一给出。
    pub(super) fn cancel_compact(&mut self) {
        if let CompactionState::Running(token) = &self.compaction {
            token.cancel();
        }
    }

    /// 压缩线程完成回调：复位压缩状态并把结果投影为会话流 note。
    pub(super) fn on_compact_finished(&mut self, result: Result<CompactionOutcome, String>) {
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
    }
}

//! 会话动作：斜杠命令执行、会话换绑与上下文压缩的异步编排。
//!
//! 换绑统一入口 [`TuiApp::rebind_conversation`] 消除 resume/new 双份
//! conversation/thread/transcript/scroll 重置逻辑；/compact 不再同步阻塞
//! 事件循环，而是返回 [`Action::Compact`] 由事件循环 spawn 后台线程执行，
//! Esc 通过外部 [`CancellationToken`] 取消本次压缩。

use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_runtime::{CompactionOutcome, Conversation, Thread, TurnRunner, TurnUsage};

use super::app::TuiApp;
use super::commands::{Action, SlashCommand};
use super::modals::{ResumeMenu, SettingsMenu};
use super::scroll::ScrollState;
use super::transcript::{NoteStyle, Transcript};
use super::view::{short_id, truncate_label};

impl TuiApp {
    /// 会话换绑统一入口：替换 conversation、thread_id、transcript、scroll
    /// 与 last_usage，消除 resume/new 双份换绑逻辑。
    fn rebind_conversation(
        &mut self,
        runner: Arc<TurnRunner>,
        thread: Thread,
        last_usage: Option<TurnUsage>,
    ) {
        let thread_id = thread.thread_id.clone();
        self.conversation = Conversation::new(runner, thread);
        self.thread_id = thread_id;
        self.transcript = Transcript::new();
        self.scroll = ScrollState::default();
        self.last_usage = last_usage;
    }

    /// 换绑到已持久化的会话；失败时只记 note，状态不变。
    pub(super) fn resume_thread(&mut self, thread_id: &str) {
        let runner = self.conversation.runner_handle();
        match singularity_runtime::resume_thread(runner.sessions_dir(), thread_id) {
            Ok(thread) => {
                let last_usage =
                    singularity_runtime::list_threads(self.conversation.runner().sessions_dir())
                        .ok()
                        .and_then(|threads| {
                            threads
                                .into_iter()
                                .find(|summary| summary.thread_id == thread.thread_id)
                        })
                        .map(|summary| TurnUsage {
                            input_tokens: 0,
                            output_tokens: 0,
                            total_tokens: summary.total_tokens,
                            cached_input_tokens: 0,
                            reasoning_tokens: 0,
                            usage_present: summary.total_tokens > 0,
                            usage_complete: false,
                        });
                let thread_id = thread.thread_id.clone();
                self.rebind_conversation(runner, thread, last_usage);
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
            SlashCommand::New => {
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
            SlashCommand::Compact => {
                if self.compacting {
                    self.transcript
                        .push_note("compaction already in progress", NoteStyle::Dim);
                } else {
                    self.compacting = true;
                    self.compact_cancel = Some(CancellationToken::new());
                    self.transcript
                        .push_note("compacting context…", NoteStyle::Dim);
                    return Action::Compact;
                }
            }
            SlashCommand::Name if !argument.trim().is_empty() => {
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
        let accepted = self.conversation.submit_follow_up(text.clone());
        self.note_injection("followUp", accepted, &text);
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

    /// 事件循环 spawn 压缩线程前取走外部取消令牌的克隆。
    pub(super) fn compact_token(&self) -> CancellationToken {
        self.compact_cancel
            .clone()
            .expect("compact token must be set before Action::Compact")
    }

    /// 压缩进行中取消本次压缩：触发令牌取消；收尾文案由
    /// [`TuiApp::on_compact_finished`] 统一给出。
    pub(super) fn cancel_compact(&mut self) {
        if let Some(token) = self.compact_cancel.as_ref() {
            token.cancel();
        }
    }

    /// 压缩线程完成回调：复位压缩状态并把结果投影为会话流 note。
    pub(super) fn on_compact_finished(&mut self, result: Result<CompactionOutcome, String>) {
        self.compacting = false;
        let cancelled = self
            .compact_cancel
            .take()
            .map(|token| token.is_cancelled())
            .unwrap_or(false);
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

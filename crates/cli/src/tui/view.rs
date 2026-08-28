//! TUI 渲染辅助单元：居中矩形、命令补全、footer 合同与标签格式化。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::app::{Phase, SPINNER_FRAMES, TuiApp};
use super::commands::SlashCommand;
use super::modals::SETTINGS_MENU_HINT;

pub(super) fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
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

pub(super) fn command_matches(prefix: &str) -> Vec<SlashCommand> {
    SlashCommand::completions(prefix).collect()
}

/// 已完成 turn 的 token 摘要（footer 与完成 note 共用）。
pub(super) fn describe_usage(turn: &singularity_runtime::objects::Turn) -> String {
    match &turn.usage {
        Some(usage) if usage.usage_present => format!(
            "{} in / {} out tokens",
            usage.input_tokens, usage.output_tokens
        ),
        _ => "usage unavailable".to_string(),
    }
}

/// 线程/会话 id 的短显示（前 8 个字符）。
pub(super) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// 把超长标签截断为「前 N-3 字符 + …」。
pub(super) fn truncate_label(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{cut}…")
}

impl TuiApp {
    /// footer 合同：状态行＝相位+spinner·具名等待对象·thread·模型·
    /// token/队列数·浏览指示（含新增计数）·压缩进行指示。提示行按上下文
    /// 给出关键操作。
    pub(super) fn footer_spans(
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
            status.push(Span::styled(format!("{spinner} running"), warn));
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
        if let Some(tokens) = self.session_tokens {
            status.push(Span::styled(format!(" {tokens} tokens"), dim));
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
        // 压缩进行中显示可见指示（后台执行，界面持续渲染）。
        if self.compaction.is_running() {
            status.push(Span::styled(" · compacting…", warn));
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
                Phase::Running => {
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
}

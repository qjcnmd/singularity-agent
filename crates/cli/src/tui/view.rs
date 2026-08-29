//! TUI 渲染辅助单元：居中矩形、命令补全、footer 合同与标签格式化。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::app::{Phase, SPINNER_FRAMES, TuiApp};
use super::commands::SlashCommand;
use super::modals::{RESUME_ARCHIVE_HINT, RESUME_MENU_HINT, SETTINGS_MENU_HINT};

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
    ///
    /// 右端预留 [stop] 宽度（running 相位）右对齐；左侧内容按 unicode 宽度
    /// 逐 span 裁剪到剩余预算，截断补 `…`（继承被截 span 样式）。
    pub(super) fn footer_spans(
        &self,
        total_rows: usize,
        viewport: usize,
        width: u16,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        const STOP_STR: &str = "[stop]";
        let stop_width = UnicodeWidthStr::width(STOP_STR) as u16;
        let available = if self.phase != Phase::Idle {
            width.saturating_sub(stop_width + 1)
        } else {
            width
        };

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

        // 按可用宽度逐 span 裁剪，截断补 …（继承被截 span 样式）。
        let mut trimmed = Vec::new();
        let mut used = 0u16;
        for span in status {
            let span_width = UnicodeWidthStr::width(span.content.as_ref()) as u16;
            let remaining = available.saturating_sub(used);
            if span_width <= remaining {
                trimmed.push(span);
                used += span_width;
            } else if remaining > 0 {
                let style = span.style.clone();
                let text = span.content.as_ref();
                let mut cut = remaining as usize;
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                let mut truncated = text[..cut].to_string();
                truncated.push('…');
                trimmed.push(Span::styled(truncated, style));
                used += remaining;
                break;
            } else {
                break;
            }
        }

        // 运行中右对齐显示 [stop]：左侧内容不足时以空白填充到右缘。
        if self.phase != Phase::Idle {
            if used < available {
                trimmed.push(Span::raw(" ".repeat((available - used) as usize)));
            }
            trimmed.push(Span::styled(
                " ".to_string(),
                Style::new(),
            ));
            trimmed.push(Span::styled(
                STOP_STR.to_string(),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }

        let hint_text = if self.quit_armed {
            "press Ctrl+C again to quit"
        } else if self.settings.is_some() {
            SETTINGS_MENU_HINT
        } else if let Some(menu) = self.resume.as_ref() {
            if menu.confirming_delete.is_some() {
                RESUME_ARCHIVE_HINT
            } else {
                RESUME_MENU_HINT
            }
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
        let hint_style = if self.quit_armed
            || self
                .resume
                .as_ref()
                .is_some_and(|menu| menu.confirming_delete.is_some())
        {
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let hint = vec![Span::styled(hint_text, hint_style)];
        (trimmed, hint)
    }
}

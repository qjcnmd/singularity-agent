//! TUI 渲染辅助单元：居中矩形、命令补全、footer 合同与标签格式化。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{Phase, SPINNER_FRAMES, TuiApp};
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

/// 把超长标签截断为「前 N-3 字符 + …」。单遍扫描：顺便判定是否超限，
/// 高频工具预览路径不再为计数与截取各扫一遍。
pub(super) fn truncate_label(text: &str, max_chars: usize) -> String {
    let keep = max_chars.saturating_sub(3);
    let mut kept_bytes = 0usize;
    let mut count = 0usize;
    let mut exceeded = false;
    for (byte, ch) in text.char_indices() {
        if count < keep {
            kept_bytes = byte + ch.len_utf8();
        }
        count += 1;
        if count > max_chars {
            exceeded = true;
            break;
        }
    }
    if !exceeded {
        return text.to_string();
    }
    let mut truncated = text[..kept_bytes].to_string();
    truncated.push('…');
    truncated
}

/// 选中反白：可视片段按片段内字符区间切 spans，选中段加 `REVERSED`。
/// 区间升序不重叠（调用方 `selection_spans` 保证）；按字符切分防宽字符
/// 撕裂，越界钳制。
pub(super) fn highlight_piece(piece: String, ranges: &[(usize, usize)]) -> Line<'static> {
    if ranges.is_empty() {
        return Line::from(Span::raw(piece));
    }
    let selected = Style::new().add_modifier(Modifier::REVERSED);
    let chars: Vec<char> = piece.chars().collect();
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for &(from, to) in ranges {
        let (from, to) = (from.min(chars.len()), to.min(chars.len()));
        if cursor < from {
            spans.push(Span::raw(chars[cursor..from].iter().collect::<String>()));
        }
        if from < to {
            spans.push(Span::styled(
                chars[from..to].iter().collect::<String>(),
                selected,
            ));
        }
        cursor = cursor.max(to);
    }
    if cursor < chars.len() {
        spans.push(Span::raw(chars[cursor..].iter().collect::<String>()));
    }
    Line::from(spans)
}

/// 状态行收尾合同：左侧内容按 unicode 宽度逐 span 裁剪到预算内，截断补
/// `…`（截断体加省略号不超过剩余预算）；running 相位在 `width` 内右对齐
/// 补 `[stop]`。返回各 span 的列宽合计恒不超过 `width`，第二元素是 `[stop]`
/// 的列宽（非 running 时 `None`），供点击命中矩形单点消费。
pub(super) fn fit_status_line(
    status: Vec<Span<'static>>,
    width: u16,
    running: bool,
) -> (Vec<Span<'static>>, Option<u16>) {
    const STOP_STR: &str = "[stop]";
    let stop_width = UnicodeWidthStr::width(STOP_STR) as u16;
    let available = if running {
        width.saturating_sub(stop_width + 1)
    } else {
        width
    };

    let mut trimmed = Vec::new();
    let mut used = 0u16;
    for span in status {
        let span_width = UnicodeWidthStr::width(span.content.as_ref()) as u16;
        let remaining = available.saturating_sub(used);
        if span_width <= remaining {
            trimmed.push(span);
            used += span_width;
        } else if remaining > 0 {
            let style = span.style;
            let text = span.content.as_ref();
            // 为省略号预留 1 列：截断体列宽 + 1 恒不超过剩余预算。
            let budget = (remaining - 1) as usize;
            let mut cut_cells = 0usize;
            let mut cut_bytes = 0usize;
            for ch in text.chars() {
                let cell = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cut_cells + cell > budget {
                    break;
                }
                cut_cells += cell;
                cut_bytes += ch.len_utf8();
            }
            let mut truncated = text[..cut_bytes].to_string();
            truncated.push('…');
            trimmed.push(Span::styled(truncated, style));
            used += cut_cells as u16 + 1;
            break;
        } else {
            break;
        }
    }

    // running 相位右对齐 [stop]：左侧内容不足时以空白填充到右缘。
    if running {
        if used < available {
            trimmed.push(Span::raw(" ".repeat((available - used) as usize)));
        }
        trimmed.push(Span::styled(" ".to_string(), Style::new()));
        trimmed.push(Span::styled(
            STOP_STR.to_string(),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        return (trimmed, Some(stop_width));
    }
    (trimmed, None)
}

impl TuiApp {
    /// footer 合同：状态行＝相位+spinner·具名等待对象·thread·模型·
    /// token/队列数·浏览指示（含新增计数）·压缩进行指示。提示行按上下文
    /// 给出关键操作。
    ///
    /// 右端预留 [stop] 宽度（running 相位）右对齐；左侧内容按 unicode 宽度
    /// 逐 span 裁剪到剩余预算，截断补 `…`（继承被截 span 样式）。第三元素是
    /// `[stop]` 列宽（非 running 时 `None`）。
    pub(super) fn footer_spans(
        &self,
        total_rows: usize,
        viewport: usize,
        width: u16,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Option<u16>) {
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
        let thread_id = self.current_thread_id();
        status.push(Span::styled(
            format!(" · thread {} · ", short_id(&thread_id)),
            dim,
        ));
        match self.conversation.thread().model {
            Some(model) => status.push(Span::styled(format!("{model} · "), dim)),
            None => status.push(Span::styled("model unset · ", warn)),
        }
        if self.transcript.thinking_collapsed() {
            status.push(Span::styled("[thinking folded]", dim));
        }
        if let Some(tokens) = self.session_tokens {
            status.push(Span::styled(format!(" {tokens} tokens"), dim));
        }
        let queue = self.conversation.pending_follow_up_count();
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
        // 压缩队列的可见计数（Alt+Up 可取回）。
        let queued = self.compaction_queue.len();
        if queued > 0 {
            status.push(Span::styled(format!(" queued:{queued}"), warn));
        }

        // 按可用宽度收尾：裁剪与 [stop] 右对齐合同见 fit_status_line。
        let (trimmed, stop_width) = fit_status_line(status, width, self.phase != Phase::Idle);

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
        } else if self.compaction.is_running() {
            "Esc cancel compaction"
        } else {
            // 一次一条当前最要紧的操作提示；完整快捷键表在 --help 与文档。
            match self.phase {
                Phase::Idle => "Enter send · / commands",
                Phase::Running => "Enter steer · Esc stop",
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
        (trimmed, hint, stop_width)
    }
}

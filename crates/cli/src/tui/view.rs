//! TUI 渲染辅助单元。

use ratatui::layout::Rect;

use super::commands::SlashCommand;

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

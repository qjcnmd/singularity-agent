//! 设置菜单与恢复会话菜单的模态面板：状态、键盘处理与渲染。
//!
//! 两个模态共享同一套范式——Enter 确认、Esc 关闭、Tab/方向键切换字段。
//! 渲染与键盘处理分别以 `impl TuiApp` 方法注入到主应用，从而访问
//! `self.settings`/`self.resume` 等私有字段。

use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use singularity_runtime::{ReasoningPatch, SettingsPatch};

use super::app::TuiApp;
use super::commands::Action;
use super::transcript::NoteStyle;
use super::view::{centered_rect, command_matches, short_id};

/// 设置菜单提示：菜单内与状态行提示共用同一文案（行为与提示同源，防漂移）。
pub(super) const SETTINGS_MENU_HINT: &str = "Enter apply · Tab next field · Esc close";

/// 设置面板的临时编辑状态。
pub(crate) struct SettingsMenu {
    pub(super) field: usize,
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) reasoning: String,
    pub(super) error: Option<String>,
}

impl SettingsMenu {
    pub fn open(current_model: Option<&str>) -> Self {
        Self::open_field(current_model, 0)
    }

    pub(super) fn open_field(current_model: Option<&str>, field: usize) -> Self {
        let parts = singularity_model::split_model_selector(current_model.unwrap_or_default());
        Self {
            field,
            provider: parts.provider.unwrap_or("openai_compatible").to_string(),
            model: parts.model.unwrap_or_default().to_string(),
            reasoning: parts.effort.unwrap_or_default().to_string(),
            error: None,
        }
    }

    pub(super) fn fields(&self) -> [&String; 3] {
        [&self.provider, &self.model, &self.reasoning]
    }

    pub(super) fn current_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.provider,
            1 => &mut self.model,
            _ => &mut self.reasoning,
        }
    }

    pub(super) fn patch(&self) -> SettingsPatch {
        let optional = |value: &String| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        SettingsPatch {
            provider: optional(&self.provider),
            model: optional(&self.model),
            reasoning: optional(&self.reasoning).map_or(ReasoningPatch::Clear, ReasoningPatch::Set),
        }
    }
}

/// 恢复会话选择菜单。
pub(crate) struct ResumeMenu {
    pub(super) threads: Vec<singularity_runtime::ThreadSummary>,
    pub(super) selected: usize,
}

// =========================================================================
// TuiApp 方法：设置菜单键盘路由
// =========================================================================

impl TuiApp {
    /// 设置菜单激活时的键盘路由。返回最终 Action（仅 `Continue`）。
    pub(super) fn handle_settings_key(&mut self, key: KeyCode) -> Action {
        let Some(menu) = self.settings.as_mut() else {
            return Action::Continue;
        };
        match key {
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
                    Ok(result)
                        if result.timing
                            == singularity_runtime::SettingsApplyTiming::NothingToApply =>
                    {
                        menu.error = Some("nothing to change".into());
                    }
                    Ok(result) => {
                        let queued_now = result.timing
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
        Action::Continue
    }
}

// =========================================================================
// TuiApp 方法：恢复菜单键盘路由
// =========================================================================

impl TuiApp {
    /// 恢复菜单激活时的键盘路由。Enter 执行换绑并关闭菜单。
    pub(super) fn handle_resume_key(&mut self, key: KeyCode) -> Action {
        let Some(menu) = self.resume.as_mut() else {
            return Action::Continue;
        };
        match key {
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
        Action::Continue
    }
}

// =========================================================================
// 渲染
// =========================================================================

impl TuiApp {
    /// 设置菜单 Popup：三个可编辑字段 + 错误提示 + 操作提示。
    pub(super) fn render_settings(&self, frame: &mut Frame<'_>, menu: &SettingsMenu) {
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

    /// 恢复会话菜单 Popup：可滚动的线程列表（最多 8 条）。
    pub(super) fn render_resume(&self, frame: &mut Frame<'_>, menu: &ResumeMenu) {
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

    /// 斜杠命令补全 Popup：光标前输入单个命令前缀时展示匹配命令。
    pub(super) fn render_command_menu(&self, frame: &mut Frame<'_>) {
        let prefix = self.editor.text();
        let matches = command_matches(&prefix);
        if matches.is_empty() {
            return;
        }
        let popup = centered_rect(frame.area(), 64, matches.len() as u16 + 2);
        frame.render_widget(Clear, popup);
        let lines = matches
            .into_iter()
            .map(|command| {
                Line::from(vec![
                    Span::styled(
                        format!("{:<12}", command.as_str()),
                        Style::new().fg(Color::Cyan),
                    ),
                    Span::styled(command.description(), Style::new().fg(Color::DarkGray)),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("commands")),
            popup,
        );
    }
}

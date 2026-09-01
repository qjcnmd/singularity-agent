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

use singularity_runtime::{ReasoningPatch, SettingsApplyTiming, SettingsPatch};

use super::app::TuiApp;
use super::commands::Action;
use super::transcript::NoteStyle;
use super::view::{centered_rect, short_id};

/// 设置菜单提示：菜单内与状态行提示共用同一文案（行为与提示同源，防漂移）。
pub(super) const SETTINGS_MENU_HINT: &str = "Enter apply · Tab next field · Esc close";

/// 恢复菜单常规提示。
pub(super) const RESUME_MENU_HINT: &str = "Enter resume · Ctrl+D archive · Esc close";

/// 恢复菜单归档确认态提示（红色），确认态只接受 Enter/Esc。
pub(super) const RESUME_ARCHIVE_HINT: &str = "Archive this session? Enter confirm · Esc cancel";

/// 设置面板的临时编辑状态。
pub(crate) struct SettingsMenu {
    pub(super) field: usize,
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) reasoning: String,
    pub(super) error: Option<String>,
    /// 命名模式：`Some(当前名)` 时面板退化为单行命名输入，Enter 走
    /// `thread_catalog.rename` 路径。
    pub(super) name: Option<String>,
}

impl SettingsMenu {
    pub fn open(current_model: Option<&str>) -> Self {
        Self::open_field(current_model, 0)
    }

    pub(super) fn open_field(current_model: Option<&str>, field: usize) -> Self {
        let parts = singularity_model::split_model_selector(current_model.unwrap_or_default());
        Self {
            field,
            provider: parts
                .provider
                .unwrap_or(singularity_model::DEFAULT_PROVIDER_NAME)
                .to_string(),
            model: parts.model.unwrap_or_default().to_string(),
            reasoning: parts.effort.unwrap_or_default().to_string(),
            error: None,
            name: None,
        }
    }

    /// 命名模式：单行编辑当前会话名。
    pub(super) fn open_name(current_name: Option<&str>) -> Self {
        Self {
            field: 0,
            provider: String::new(),
            model: String::new(),
            reasoning: String::new(),
            error: None,
            name: Some(current_name.unwrap_or_default().to_string()),
        }
    }

    pub(super) fn is_name_mode(&self) -> bool {
        self.name.is_some()
    }

    pub(super) fn fields(&self) -> [&String; 3] {
        [&self.provider, &self.model, &self.reasoning]
    }

    pub(super) fn current_mut(&mut self) -> &mut String {
        if let Some(name) = &mut self.name {
            return name;
        }
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

/// 恢复菜单一屏可见的列表行数。
const RESUME_WINDOW_ROWS: usize = 8;

/// 恢复会话选择菜单（列表项为头部元数据）。
pub(crate) struct ResumeMenu {
    pub(super) threads: Vec<singularity_runtime::ThreadListing>,
    pub(super) selected: usize,
    /// 可见窗口首项下标：随 selected 推进，保证选中行始终可见。
    pub(super) offset: usize,
    /// 归档两阶段确认：`Some(thread_id)` 表示该行正等待 Enter 确认或 Esc 取消。
    pub(super) confirming_delete: Option<String>,
    /// 菜单内错误提示（如拒绝归档当前活动会话）。
    pub(super) error: Option<String>,
}

impl ResumeMenu {
    /// 以会话列表构造并复位确认/错误态。
    pub(super) fn new(threads: Vec<singularity_runtime::ThreadListing>) -> Self {
        Self {
            threads,
            selected: 0,
            offset: 0,
            confirming_delete: None,
            error: None,
        }
    }

    /// 选中行上移一条，窗口随之回退。
    pub(super) fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.reveal_selected();
    }

    /// 选中行下移一条（钳制在列表末行），窗口随之推进。
    pub(super) fn select_down(&mut self) {
        self.selected = (self.selected + 1).min(self.threads.len().saturating_sub(1));
        self.reveal_selected();
    }

    fn reveal_selected(&mut self) {
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + RESUME_WINDOW_ROWS {
            self.offset = self.selected + 1 - RESUME_WINDOW_ROWS;
        }
    }
}

// =========================================================================
// TuiApp 方法：设置菜单键盘路由
// =========================================================================

impl TuiApp {
    /// 设置/命名菜单激活时的键盘路由。返回最终 Action（仅 `Continue`）。
    /// 字符输入与主路径同一修饰键守卫：Ctrl/Alt 组合不落入字段。
    pub(super) fn handle_settings_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::KeyModifiers;
        let Some(menu) = self.settings.as_mut() else {
            return Action::Continue;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.settings = None;
            }
            KeyCode::Tab if !menu.is_name_mode() => menu.field = (menu.field + 1) % 3,
            KeyCode::Backspace => {
                menu.current_mut().pop();
            }
            KeyCode::Enter if menu.is_name_mode() => {
                let name = menu.current_mut().trim().to_string();
                let rename = if self.conversation.has_active_turn() {
                    Err(singularity_runtime::ConversationError::TurnAlreadyActive.to_string())
                } else {
                    let thread_id = self.conversation.thread().thread_id;
                    self.thread_catalog.rename(&thread_id, &name)
                };
                match rename {
                    Ok(()) => self
                        .transcript
                        .push_note(format!("session named {name}"), NoteStyle::Accent),
                    Err(error) => menu.error = Some(error),
                }
                self.settings = None;
            }
            KeyCode::Enter => {
                let patch = menu.patch();
                match self.conversation.update_settings(patch) {
                    Ok(SettingsApplyTiming::NothingToApply) => {
                        menu.error = Some("nothing to change".into());
                    }
                    Ok(_) => {
                        self.transcript
                            .push_note("settings updated for this thread", NoteStyle::Accent);
                        self.settings = None;
                    }
                    Err(error) => menu.error = Some(error.to_string()),
                }
            }
            KeyCode::Char(ch) if !ctrl && !alt => menu.current_mut().push(ch),
            _ => {}
        }
        Action::Continue
    }

    /// 设置/命名菜单激活时的括号粘贴：单行字段只接受清洗后的内容
    /// （换行与回车剔除），落入当前字段。
    pub(super) fn handle_settings_paste(&mut self, text: String) {
        let Some(menu) = self.settings.as_mut() else {
            return;
        };
        let cleaned: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        menu.current_mut().push_str(&cleaned);
    }
}

// =========================================================================
// TuiApp 方法：恢复菜单键盘路由
// =========================================================================

impl TuiApp {
    /// 恢复菜单激活时的键盘路由。Enter 执行换绑并关闭菜单；Ctrl+D 触发归档
    /// 两阶段确认，确认态只接受 Enter（归档）与 Esc（取消），其余键忽略。
    pub(super) fn handle_resume_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::KeyModifiers;
        let Some(menu) = self.resume.as_mut() else {
            return Action::Continue;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // 确认态优先：只接受 Enter 归档、Esc 取消（拦截其它键）。
        if let Some(target) = menu.confirming_delete.clone() {
            match key.code {
                KeyCode::Enter => {
                    // 先清确认态再执行，避免归档失败后仍停在确认帧。
                    menu.confirming_delete = None;
                    menu.error = None;
                    match self.thread_catalog.archive(&target) {
                        Ok(()) => {
                            self.transcript.push_note(
                                format!("archived session {}", short_id(&target)),
                                NoteStyle::Accent,
                            );
                            // 重新拉取列表：归档的会话从目录消失（可能已空）。
                            self.resume = match self.thread_catalog.list_threads() {
                                Ok(threads) if !threads.is_empty() => {
                                    Some(ResumeMenu::new(threads))
                                }
                                _ => None,
                            };
                        }
                        Err(error) => {
                            menu.error = Some(format!("archive failed: {error}"));
                        }
                    }
                }
                KeyCode::Esc => {
                    menu.confirming_delete = None;
                    menu.error = None;
                }
                _ => {}
            }
            return Action::Continue;
        }
        match key.code {
            KeyCode::Esc => self.resume = None,
            KeyCode::Up => menu.select_up(),
            KeyCode::Down => menu.select_down(),
            KeyCode::Enter => {
                let selected = menu.threads.get(menu.selected).cloned();
                self.resume = None;
                if let Some(summary) = selected {
                    self.resume_thread(&summary.thread_id);
                }
            }
            KeyCode::Char('d') if ctrl => {
                let Some(target) = menu.threads.get(menu.selected) else {
                    return Action::Continue;
                };
                // 拒绝归档当前活动会话。
                let active_thread = self.conversation.thread().thread_id == target.thread_id;
                if active_thread {
                    menu.error = Some("cannot archive the active session".to_string());
                    return Action::Continue;
                }
                menu.error = None;
                menu.confirming_delete = Some(target.thread_id.clone());
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
    /// 命名模式退化为单行名称字段。
    pub(super) fn render_settings(frame: &mut Frame<'_>, menu: &SettingsMenu) {
        let popup = centered_rect(frame.area(), 60, 9);
        frame.render_widget(Clear, popup);
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(name) = &menu.name {
            lines.push(Line::from(Span::styled(
                format!("name: {name}"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )));
            if let Some(error) = &menu.error {
                lines.push(Line::from(Span::styled(
                    error.clone(),
                    Style::new().fg(Color::Red),
                )));
            }
            lines.push(Line::from(Span::styled(
                "Enter apply · Esc close",
                Style::new().fg(Color::DarkGray),
            )));
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::default().borders(Borders::ALL).title("session name")),
                popup,
            );
            return;
        }
        let names = ["provider", "model", "reasoning"];
        let values = menu.fields();
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

    /// 恢复会话菜单 Popup：窗口化线程列表（一屏最多 8 行，选中行始终可见），
    /// 标题带 `(选中/总数)`。确认态把目标行标红，有错误时追加一行红色提示。
    pub(super) fn render_resume(frame: &mut Frame<'_>, menu: &ResumeMenu) {
        let error_lines = usize::from(menu.error.is_some());
        let visible_rows = menu.threads.len().min(RESUME_WINDOW_ROWS);
        let height = (visible_rows as u16)
            .saturating_add(2 + error_lines as u16)
            .max(3);
        let popup = centered_rect(frame.area(), 72, height);
        frame.render_widget(Clear, popup);
        let mut lines = menu.threads[menu.offset..]
            .iter()
            .take(RESUME_WINDOW_ROWS)
            .enumerate()
            .map(|(row, thread)| {
                let index = menu.offset + row;
                let confirming = menu
                    .confirming_delete
                    .as_deref()
                    .is_some_and(|target| target == thread.thread_id);
                let style = if confirming {
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else if index == menu.selected {
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::DarkGray)
                };
                let marker = if confirming { "▸ " } else { "  " };
                Line::from(Span::styled(
                    format!(
                        "{}{} · {}",
                        marker,
                        short_id(&thread.thread_id),
                        thread.updated_at
                    ),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        if let Some(error) = &menu.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(Color::Red),
            )));
        }
        let title = format!("resume ({}/{})", menu.selected + 1, menu.threads.len());
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
            popup,
        );
    }

    /// 斜杠命令补全 Popup：光标前输入单个命令前缀时展示匹配命令。
    pub(super) fn render_command_menu(&self, frame: &mut Frame<'_>) {
        let prefix = self.editor.text();
        let matches = super::commands::SlashCommand::completions(&prefix).collect::<Vec<_>>();
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

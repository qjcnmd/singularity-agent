//! 交互式 TUI：主会话流 + 底部多行编辑器 + 状态 footer + 临时选择菜单。
//!
//! TUI 只依赖 [`singularity_runtime`] 的 `Conversation` 与 `TurnEvent`：
//! turn 在工作线程上执行，事件经通道驱动渲染；输入在活动 turn 期间按当前
//! 模式进入 steer（立即注入）或 followUp（完成后排队下一轮）。第一次
//! Ctrl+C 中断当前 turn，第二次强制退出；退出路径统一恢复终端状态。

use std::io::{IsTerminal, Stdout, Write};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use singularity_runtime::Conversation;
use singularity_runtime::events::{TurnEvent, TurnEventSink};
use singularity_runtime::objects::TurnStatus;

const INTERRUPT_POLL: Duration = Duration::from_millis(100);
const TOOL_RESULT_PREVIEW_CHARS: usize = 600;

pub struct InteractiveOutcome {
    pub exit_code: i32,
}

/// 交互模式要求真实终端；不满足时返回面向用户的诊断。
pub fn ensure_terminal() -> Result<(), &'static str> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Ok(())
    } else {
        Err(
            "interactive mode requires a terminal; use `sg --print <goal>` or `sg --json <goal>` for non-interactive execution",
        )
    }
}

/// 进入长驻交互模式。调用方必须先通过 [`ensure_terminal`]。
pub fn run(conversation: Arc<Conversation>) -> InteractiveOutcome {
    match run_inner(conversation) {
        Ok(code) => InteractiveOutcome { exit_code: code },
        Err(error) => {
            let _ = restore_terminal();
            eprintln!("sg: {error}");
            InteractiveOutcome { exit_code: 1 }
        }
    }
}

// ---------------------------------------------------------------------------
// 终端生命周期
// ---------------------------------------------------------------------------

fn enter_alternate_screen() -> std::io::Result<()> {
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
    enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    Ok(())
}

fn restore_terminal() -> std::io::Result<()> {
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
    let _ = disable_raw_mode();
    crossterm::execute!(
        std::io::stdout(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    std::io::stdout().flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// UI 状态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Steer,
    FollowUp,
}

impl InputMode {
    fn toggled(self) -> Self {
        match self {
            Self::Steer => Self::FollowUp,
            Self::FollowUp => Self::Steer,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnPhase {
    Idle,
    Running,
    Interrupting,
}

/// 会话流中的一行文本（已样式化）。
type FlowLine = Line<'static>;

/// 主会话流投影状态：把 typed 事件合并成可读段落。
#[derive(Default)]
struct Transcript {
    lines: Vec<FlowLine>,
    /// 当前正在累积的 assistant 段落缓冲。
    assistant_buffer: String,
    assistant_active: bool,
}

impl Transcript {
    fn push_plain(&mut self, text: impl Into<String>, style: Style) {
        self.flush_assistant_paragraph();
        self.lines
            .push(Line::from(Span::styled(text.into(), style)));
    }

    fn assistant_delta(&mut self, delta: &str) {
        if !self.assistant_active {
            self.flush_assistant_paragraph();
            self.assistant_active = true;
        }
        self.assistant_buffer.push_str(delta);
    }

    fn flush_assistant_paragraph(&mut self) {
        if !self.assistant_buffer.is_empty() || self.assistant_active {
            let text = std::mem::take(&mut self.assistant_buffer);
            self.lines.push(Line::from(Span::raw(text)));
            self.assistant_active = false;
        }
    }
}

/// 把一个事件投影进会话流；assistant 增量只累积，遇到其他事实时先落段。
fn project_event(transcript: &mut Transcript, event: &TurnEvent) {
    const DIM: Style = Style::new().fg(Color::DarkGray);
    const TOOL: Style = Style::new().fg(Color::Cyan);
    const WARN: Style = Style::new().fg(Color::Yellow);
    const ERROR: Style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    if !matches!(event, TurnEvent::AssistantDelta { .. }) {
        transcript.flush_assistant_paragraph();
    }
    match event {
        TurnEvent::ThreadStarted { thread } => {
            transcript.push_plain(format!("thread {}", thread.thread_id), DIM);
        }
        TurnEvent::TurnStarted { turn } => {
            transcript.push_plain(
                format!("── turn {} ──", &turn.turn_id[..8.min(turn.turn_id.len())]),
                DIM,
            );
        }
        TurnEvent::AssistantDelta { delta, .. } => transcript.assistant_delta(delta),
        TurnEvent::ItemStarted { item_id, .. } => {
            // assistant item 由段落本身表达；工具 item 在 tool/execution 事件中表达。
            if !item_id.contains("_assistant") {
                // no-op；真实内容随后续工具事件到达。
            }
        }
        TurnEvent::ToolExecutionStart {
            tool_name, args, ..
        } => {
            transcript.push_plain(format!("▸ {tool_name} {}", truncate_json_args(args)), TOOL);
        }
        TurnEvent::ToolExecutionUpdate { .. } => {
            // 增量输出不逐帧上屏：结束事件携带结果预览，避免刷屏。
        }
        TurnEvent::ToolExecutionEnd {
            tool_name,
            result,
            is_error,
            ..
        } => {
            let style = if *is_error { ERROR } else { DIM };
            transcript.push_plain(tool_result_line(tool_name, result, *is_error), style);
        }
        TurnEvent::ItemCompleted { .. } | TurnEvent::ItemFailed { .. } => {}
        TurnEvent::Diagnostic {
            severity,
            code,
            message,
            ..
        } => {
            let style = match severity.as_str() {
                "error" => ERROR,
                "warning" => WARN,
                _ => DIM,
            };
            transcript.push_plain(format!("⚠ [{severity}] {code}: {message}"), style);
        }
        TurnEvent::ProviderAttempt { .. } | TurnEvent::ProviderAttemptSummary { .. } => {}
        TurnEvent::TurnCompleted { turn } => {
            transcript.push_plain(format!("✔ completed ({})", describe_usage(turn)), DIM);
        }
        TurnEvent::TurnFailed { error, .. } => {
            transcript.push_plain(
                format!("✖ failed [{}]: {}", error.cause, error.message),
                ERROR,
            );
        }
    }
}

fn truncate_json_args(args: &serde_json::Value) -> String {
    let text = serde_json::to_string(args).unwrap_or_default();
    if text.chars().count() > 120 {
        let cut: String = text.chars().take(117).collect();
        format!("{cut}...")
    } else {
        text
    }
}

fn tool_result_line(tool_name: &str, result: &str, is_error: bool) -> String {
    let marker = if is_error { "✖" } else { "·" };
    let mut preview: String = result
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(4)
        .map(|line| format!("│ {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let total_chars = result.chars().count();
    if total_chars > TOOL_RESULT_PREVIEW_CHARS && preview.len() < total_chars {
        preview.push_str("\n│ …");
    }
    if preview.is_empty() {
        format!("{marker} {tool_name} finished (no output)")
    } else {
        format!("{marker} {tool_name}\n{preview}")
    }
}

fn describe_usage(turn: &singularity_runtime::objects::Turn) -> String {
    match &turn.usage {
        Some(usage) if usage.usage_present => {
            format!(
                "{} in / {} out tokens",
                usage.input_tokens, usage.output_tokens
            )
        }
        _ => "usage unavailable".to_string(),
    }
}

// ---------------------------------------------------------------------------
// 设置菜单（临时弹出）
// ---------------------------------------------------------------------------

struct SettingsMenu {
    field: usize,
    provider: String,
    model: String,
    reasoning: String,
    error: Option<String>,
}

impl SettingsMenu {
    fn open(current_model: Option<&str>) -> Self {
        let parts = singularity_model::split_model_selector(current_model.unwrap_or_default());
        Self {
            field: 0,
            provider: parts.provider.unwrap_or("openai_compatible").to_string(),
            model: parts.model.unwrap_or_default().to_string(),
            reasoning: parts.effort.unwrap_or_default().to_string(),
            error: None,
        }
    }

    fn fields(&self) -> [&String; 3] {
        [&self.provider, &self.model, &self.reasoning]
    }

    fn current_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.provider,
            1 => &mut self.model,
            _ => &mut self.reasoning,
        }
    }

    fn patch(&self) -> singularity_runtime::SettingsPatch {
        let optional = |value: &String| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        singularity_runtime::SettingsPatch {
            provider: optional(&self.provider),
            model: optional(&self.model),
            reasoning: optional(&self.reasoning),
        }
    }
}

// ---------------------------------------------------------------------------
// 主循环
// ---------------------------------------------------------------------------

enum UiEvent {
    FromTurn(TurnEvent),
    TurnFinished(Result<singularity_runtime::TurnOutcome, String>),
}

fn run_inner(conversation: Arc<Conversation>) -> Result<i32, String> {
    enter_alternate_screen().map_err(|error| format!("terminal setup failed: {error}"))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|error| format!("terminal backend failed: {error}"))?;

    let mut app = App {
        conversation: Arc::clone(&conversation),
        transcript: Transcript::default(),
        input: String::new(),
        input_mode: InputMode::Steer,
        phase: TurnPhase::Idle,
        settings: None,
        followups: Vec::new(),
        thread_id: conversation.thread().ok().map(|t| t.thread_id),
        quit_hint: false,
        status_note: None,
        rx: mpsc::channel().1,
    };

    let outcome = app.event_loop(&mut terminal);
    let _ = restore_terminal();
    outcome
}

struct App {
    conversation: Arc<Conversation>,
    transcript: Transcript,
    input: String,
    input_mode: InputMode,
    phase: TurnPhase,
    settings: Option<SettingsMenu>,
    followups: Vec<String>,
    thread_id: Option<String>,
    quit_hint: bool,
    status_note: Option<&'static str>,
    rx: mpsc::Receiver<UiEvent>,
}

impl App {
    fn current_model(&self) -> Option<String> {
        self.conversation.thread().ok().and_then(|t| t.model)
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<i32, String> {
        loop {
            // 排空本轮 UI 事件。
            while let Ok(event) = self.rx.try_recv() {
                match event {
                    UiEvent::FromTurn(turn_event) => {
                        project_event(&mut self.transcript, &turn_event)
                    }
                    UiEvent::TurnFinished(result) => {
                        self.phase = TurnPhase::Idle;
                        crate::signal::reset();
                        self.status_note = None;
                        if let Ok(outcome) = &result
                            && outcome.turn_status == TurnStatus::Interrupted
                        {
                            self.transcript.push_plain(
                                "turn interrupted".to_string(),
                                Style::new().fg(Color::Yellow),
                            );
                        }
                        if let Err(message) = &result {
                            self.transcript
                                .push_plain(format!("✖ {message}"), Style::new().fg(Color::Red));
                        }
                        // followUp 队列：上一轮结束后自动启动下一轮。
                        if let Some(next) =
                            (!self.followups.is_empty()).then(|| self.followups.remove(0))
                        {
                            self.start_turn(next)?;
                        }
                    }
                }
            }

            // 键盘与终端事件。
            if crossterm::event::poll(INTERRUPT_POLL)
                .map_err(|error| format!("event poll failed: {error}"))?
            {
                let event = crossterm::event::read()
                    .map_err(|error| format!("event read failed: {error}"))?;
                match event {
                    crossterm::event::Event::Key(key)
                        if key.kind == crossterm::event::KeyEventKind::Press =>
                    {
                        if self.handle_key(key)? {
                            return Ok(0);
                        }
                    }
                    crossterm::event::Event::Resize(_, _) => {}
                    _ => {}
                }
            }

            // Ctrl+C 语义：运行中一次中断、两次强退；空闲两次退出。
            match crate::signal::count() {
                count if count >= 2 => {
                    let code = if self.phase == TurnPhase::Idle {
                        0
                    } else {
                        130
                    };
                    return Ok(code);
                }
                1 if self.phase == TurnPhase::Running => {
                    self.phase = TurnPhase::Interrupting;
                    self.conversation.interrupt();
                    self.status_note = Some("interrupting… press Ctrl+C again to force exit");
                }
                _ => {}
            }

            terminal
                .draw(|frame| self.render(frame))
                .map_err(|error| format!("draw failed: {error}"))?;
        }
    }

    /// 返回 true 表示应正常退出。
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Result<bool, String> {
        use crossterm::event::{KeyCode, KeyModifiers};
        if let Some(menu) = self.settings.as_mut() {
            match key.code {
                KeyCode::Esc => self.settings = None,
                KeyCode::Tab => menu.field = (menu.field + 1) % 3,
                KeyCode::Backspace => {
                    menu.current_mut().pop();
                }
                KeyCode::Enter => {
                    let patch = menu.patch();
                    match self.conversation.queue_settings(patch.clone()) {
                        Ok(true) => {
                            let queued_now = self.phase != TurnPhase::Idle;
                            self.transcript.push_plain(
                                if queued_now {
                                    "settings queued; effective from the next turn".to_string()
                                } else {
                                    "settings updated for this thread".to_string()
                                },
                                Style::new().fg(Color::Cyan),
                            );
                            self.settings = None;
                        }
                        Ok(false) => {
                            menu.error = Some("nothing to change".into());
                        }
                        Err(error) => menu.error = Some(error.to_string()),
                    }
                }
                KeyCode::Char(ch) => menu.current_mut().push(ch),
                _ => {}
            }
            return Ok(false);
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.phase == TurnPhase::Idle {
                    if self.quit_hint {
                        return Ok(true);
                    }
                    self.quit_hint = true;
                    self.status_note = Some("press Ctrl+C again to quit");
                }
                // 运行中的两级语义由信号计数分支处理。
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input_mode = self.input_mode.toggled();
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.settings = Some(SettingsMenu::open(self.current_model().as_deref()));
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.push('\n');
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input).trim().to_string();
                if !text.is_empty() {
                    match self.phase {
                        TurnPhase::Idle => self.start_turn(text)?,
                        TurnPhase::Running | TurnPhase::Interrupting => {
                            match self.input_mode {
                                InputMode::Steer => {
                                    let accepted = self.conversation.steer(text.clone());
                                    self.note_injection("steer", accepted, &text);
                                }
                                InputMode::FollowUp => {
                                    let accepted = self.conversation.follow_up(text.clone());
                                    if accepted {
                                        self.followups.push(text.clone());
                                        self.note_injection("followUp", true, &text);
                                    } else {
                                        // 注入窗口已关闭：排队到下一轮启动。
                                        self.followups.push(text.clone());
                                        self.transcript.push_plain(
                                            "queued as the next turn".to_string(),
                                            Style::new().fg(Color::Cyan),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(ch) => self.input.push(ch),
            _ => {}
        }
        Ok(false)
    }

    fn note_injection(&mut self, kind: &str, accepted: bool, text: &str) {
        let label = if text.chars().count() > 40 {
            let cut: String = text.chars().take(37).collect();
            format!("{cut}…")
        } else {
            text.to_string()
        };
        self.transcript.push_plain(
            if accepted {
                format!("↳ {kind}: {label}")
            } else {
                format!("↳ {kind} rejected (turn closed): {label}")
            },
            Style::new().fg(Color::Cyan),
        );
    }

    fn start_turn(&mut self, goal: String) -> Result<(), String> {
        self.phase = TurnPhase::Running;
        self.quit_hint = false;
        crate::signal::reset();
        let (tx, rx) = mpsc::channel::<UiEvent>();
        self.rx = rx;
        struct Forward {
            tx: mpsc::Sender<UiEvent>,
        }
        impl TurnEventSink for Forward {
            fn emit(&mut self, event: TurnEvent) {
                let _ = self.tx.send(UiEvent::FromTurn(event));
            }
        }
        let conversation = Arc::clone(&self.conversation);
        std::thread::spawn(move || {
            let mut sink = Forward { tx };
            let result = conversation.run_turn(&goal, &mut sink);
            let _ = sink.tx.send(UiEvent::TurnFinished(match result {
                Ok(outcome) => Ok(outcome),
                Err(error) => Err(error.to_string()),
            }));
        });
        Ok(())
    }

    // 渲染 ----------------------------------------------------------------

    fn render(&self, frame: &mut ratatui::Frame) {
        let [flow, editor, footer] = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let mut flow_lines = self.transcript.lines.clone();
        flow_lines.push(Line::from(""));
        let flow_widget = Paragraph::new(flow_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::NONE));
        frame.render_widget(flow_widget, flow);

        let prompt = match self.phase {
            TurnPhase::Idle => "> ",
            _ => "▌ ",
        };
        let editor_text = format!("{prompt}{}", self.input.replace('\n', "\n  "));
        let editor_style = if self.phase == TurnPhase::Idle {
            Style::new()
        } else {
            Style::new().fg(Color::DarkGray)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(editor_text, editor_style)))
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("input")),
            editor,
        );

        frame.render_widget(
            Paragraph::new(vec![Line::from(self.footer_spans())]),
            footer,
        );

        if let Some(menu) = &self.settings {
            self.render_settings(frame, menu);
        }
    }

    fn footer_spans(&self) -> Vec<Span<'static>> {
        let dim = Style::new().fg(Color::DarkGray);
        let accent = Style::new().fg(Color::Cyan);
        let warn = Style::new().fg(Color::Yellow);
        let mut spans = vec![];
        if let Some(thread_id) = &self.thread_id {
            spans.push(Span::styled(
                format!("thread {} ", &thread_id[..8.min(thread_id.len())]),
                dim,
            ));
        }
        if let Some(model) = self.current_model() {
            spans.push(Span::styled(format!("· {model} "), dim));
        }
        spans.push(Span::styled(
            match self.phase {
                TurnPhase::Idle => "· idle ".to_string(),
                TurnPhase::Running => "· running ".to_string(),
                TurnPhase::Interrupting => "· interrupting ".to_string(),
            },
            if self.phase == TurnPhase::Idle {
                dim
            } else {
                warn
            },
        ));
        spans.push(Span::styled(
            format!("· [{}] ", self.input_mode.label()),
            accent,
        ));
        if let Some(note) = self.status_note {
            spans.push(Span::styled(note.to_string(), warn));
        } else {
            spans.push(Span::styled(
                "Enter send · Ctrl+T steer/followUp · Ctrl+S settings",
                dim,
            ));
        }
        spans
    }

    fn render_settings(&self, frame: &mut ratatui::Frame, menu: &SettingsMenu) {
        let area = centered_rect(frame.area(), 60, 9);
        frame.render_widget(ratatui::widgets::Clear, area);
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
            "Enter apply · Tab next field · Esc close",
            Style::new().fg(Color::DarkGray),
        )));
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("thread settings"),
            ),
            area,
        );
    }
}

fn centered_rect(
    area: ratatui::layout::Rect,
    percent_x: u16,
    height: u16,
) -> ratatui::layout::Rect {
    let width = area.width.saturating_mul(percent_x) / 100;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    ratatui::layout::Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_deltas_merge_into_one_paragraph() {
        let mut transcript = Transcript::default();
        let delta = |text: &str| TurnEvent::AssistantDelta {
            thread_id: "t".into(),
            turn_id: "r".into(),
            item_id: "i".into(),
            delta: text.to_string(),
        };
        project_event(&mut transcript, &delta("Hel"));
        project_event(&mut transcript, &delta("lo"));
        // 其他事实到达时段落落定。
        transcript.push_plain("marker", Style::new());
        let rendered: Vec<String> = transcript
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert_eq!(rendered[0], "Hello");
        assert_eq!(rendered[1], "marker");
    }

    #[test]
    fn tool_end_renders_error_marker_and_preview() {
        let line = tool_result_line("bash", "line1\nline2", true);
        assert!(line.starts_with('✖'));
        assert!(line.contains("line1"));
        let line = tool_result_line("read", "", false);
        assert!(line.contains("no output"));
    }

    #[test]
    fn settings_menu_composes_patch_from_current_selector() {
        let menu = SettingsMenu::open(Some("openai_compatible/gpt#high"));
        let patch = menu.patch();
        assert_eq!(patch.provider.as_deref(), Some("openai_compatible"));
        assert_eq!(patch.model.as_deref(), Some("gpt"));
        assert_eq!(patch.reasoning.as_deref(), Some("high"));
        let empty = SettingsMenu::open(None);
        assert_eq!(empty.patch().model.as_deref(), None);
    }

    #[test]
    fn input_mode_toggles_between_steer_and_follow_up() {
        assert_eq!(InputMode::Steer.toggled(), InputMode::FollowUp);
        assert_eq!(InputMode::FollowUp.toggled(), InputMode::Steer);
    }
}

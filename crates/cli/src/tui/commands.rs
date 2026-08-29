//! TUI 斜杠命令模型与补全。

use singularity_core::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlashCommand {
    Model,
    Settings,
    Resume,
    New,
    Session,
    Compact,
    Name,
}

impl SlashCommand {
    pub(crate) const ALL: [Self; 7] = [
        Self::Model,
        Self::Settings,
        Self::Resume,
        Self::New,
        Self::Session,
        Self::Compact,
        Self::Name,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "/model",
            Self::Settings => "/settings",
            Self::Resume => "/resume",
            Self::New => "/new",
            Self::Session => "/session",
            Self::Compact => "/compact",
            Self::Name => "/name",
        }
    }

    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Model => "select the thread model",
            Self::Settings => "edit provider, model, and reasoning",
            Self::Resume => "resume a saved session",
            Self::New => "start a new session",
            Self::Session => "show session facts",
            Self::Compact => "compact context now",
            Self::Name => "name this session",
        }
    }

    /// 整行精确匹配：返回命中的命令（无参数）；非精确命令返回 None。
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.as_str() == text)
    }

    pub(crate) fn completions(prefix: &str) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |item| item.as_str().starts_with(prefix))
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Action {
    Continue,
    Submit(String),
    Exit(i32),
    /// 后台执行上下文压缩（/compact），携带本次压缩的外部取消令牌；
    /// 事件循环负责 spawn 线程并转发结果。
    Compact(CancellationToken),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_by_exact_match() {
        assert_eq!(SlashCommand::parse("/name"), Some(SlashCommand::Name));
        assert_eq!(SlashCommand::parse("/name hello world"), None);
        assert_eq!(SlashCommand::parse("/unknown"), None);
    }

    #[test]
    fn completes_by_prefix() {
        let values = SlashCommand::completions("/s").collect::<Vec<_>>();
        assert_eq!(values, vec![SlashCommand::Settings, SlashCommand::Session]);
    }
}

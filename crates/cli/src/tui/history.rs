//! 编辑器输入历史回溯：会话内内存，不持久化。
//!
//! 提交（含 steer/followUp 成功路径）时记录，↑/↓ 回溯；进入回溯前暂存
//! 草稿，退出且未编辑时恢复。

/// 输入历史栈：纯状态机，不依赖编辑器或 UI 状态。
#[derive(Debug, Default)]
pub(crate) struct InputHistory {
    entries: Vec<String>,
    cursor: Option<usize>,
    draft: Option<String>,
}

impl InputHistory {
    /// 历史是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 是否正在回溯中。
    pub fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }

    /// 记录一条提交文本，复位回溯指针与草稿。相邻重复条目折叠。
    pub fn record(&mut self, text: &str) {
        self.cursor = None;
        self.draft = None;
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.entries.last().is_some_and(|prev| prev == text) {
            return;
        }
        self.entries.push(text.to_string());
    }

    /// 进入回溯：暂存当前草稿，返回最新条目。历史为空时返回 `None`。
    pub fn enter(&mut self, draft: String) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        self.draft = Some(draft);
        self.cursor = Some(self.entries.len() - 1);
        self.entries.last().map(String::as_str)
    }

    /// 上一条（更旧）。已在最旧条目时返回 `None`。
    pub fn up(&mut self) -> Option<&str> {
        let idx = self.cursor?;
        if idx == 0 {
            return None;
        }
        self.cursor = Some(idx - 1);
        self.entries.get(idx - 1).map(String::as_str)
    }

    /// 下一条（更新）。已到最新条目时退出回溯（游标置 `None`，返回
    /// `None`，调用方应恢复草稿）。
    pub fn down(&mut self) -> Option<&str> {
        let idx = self.cursor?;
        if idx + 1 >= self.entries.len() {
            // 已到最新：退出回溯。
            self.cursor = None;
            return None;
        }
        self.cursor = Some(idx + 1);
        self.entries.get(idx + 1).map(String::as_str)
    }

    /// 编辑退出回溯，丢弃草稿，保留当前内容。
    pub fn exit_keeping(&mut self) {
        self.cursor = None;
        self.draft = None;
    }

    /// 取走草稿（供恢复编辑器内容），草稿被清空。
    pub fn take_draft(&mut self) -> Option<String> {
        self.draft.take()
    }
}

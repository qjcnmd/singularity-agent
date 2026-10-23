//! `ask_user_question` 工具的问题、选项与用户回答类型。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserQuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserQuestion {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<UserQuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct PendingQuestion {
    /// 提问工具的条目 ID；用于拒绝已经结束或属于其他回合的问题的答案。
    pub item_id: String,
    pub questions: Vec<UserQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserQuestionAnswer {
    pub id: String,
    pub selected: Vec<String>,
    pub text: String,
    #[serde(default)]
    pub skipped: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionAnswerParams {
    pub session_id: String,
    pub item_id: String,
    pub answers: Vec<UserQuestionAnswer>,
}

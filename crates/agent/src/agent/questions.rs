//! 一个活动回合的待答问题。请求只在内存中等待；问题与答案由既有工具调用/结果账本保存。

use std::sync::Mutex;

use singularity_protocol::{PendingQuestion, UserQuestionAnswer};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::AgentEvent;
use crate::tools::{ABORTED_MESSAGE, ToolExecution, error_result};

struct WaitingQuestion {
    request: PendingQuestion,
    reply: oneshot::Sender<Vec<UserQuestionAnswer>>,
}

/// 桌面回合控制面。一个独占工具调用最多等待一组问题。
#[derive(Default)]
pub struct UserQuestions {
    waiting: Mutex<Option<WaitingQuestion>>,
}

impl UserQuestions {
    /// 刷新或切换任务时读取当前仍可回答的问题。
    pub fn pending(&self) -> Option<PendingQuestion> {
        self.waiting
            .lock()
            .expect("question lock poisoned")
            .as_ref()
            .map(|waiting| waiting.request.clone())
    }

    /// 校验界面提交的答案后交付一次；错误答案保留原问题供修正。
    pub fn answer(&self, item_id: &str, answers: Vec<UserQuestionAnswer>) -> Result<(), String> {
        let mut slot = self.waiting.lock().expect("question lock poisoned");
        let waiting = slot
            .as_ref()
            .filter(|waiting| waiting.request.item_id == item_id)
            .ok_or("该问题已结束，请查看当前任务。")?;
        if answers.len() != waiting.request.questions.len() {
            return Err("请回答所有问题。".into());
        }
        for question in &waiting.request.questions {
            let matching: Vec<_> = answers.iter().filter(|answer| answer.id == question.id).collect();
            if matching.len() != 1 {
                return Err("答案与问题不匹配。".into());
            }
            let answer = matching[0];
            if answer.skipped {
                if !answer.selected.is_empty() || !answer.text.is_empty() {
                    return Err("跳过的问题不能同时包含答案。".into());
                }
                continue;
            }
            let mut selected = std::collections::HashSet::new();
            if (!question.multi_select && answer.selected.len() > 1)
                || answer.selected.iter().any(|label| {
                    !selected.insert(label) || !question.options.iter().any(|option| &option.label == label)
                })
                || (answer.selected.is_empty() && answer.text.trim().is_empty())
            {
                return Err("请选择有效选项或填写回答。".into());
            }
        }
        slot.take()
            .expect("validated pending question")
            .reply
            .send(answers)
            .map_err(|_| "该问题已结束，请查看当前任务。".into())
    }

    pub(super) async fn ask(
        &self,
        request: PendingQuestion,
        cancellation: &CancellationToken,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> ToolExecution {
        let (reply, receive) = oneshot::channel();
        *self.waiting.lock().expect("question lock poisoned") = Some(WaitingQuestion { request, reply });
        on_event(AgentEvent::ControlChanged);
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => error_result(ABORTED_MESSAGE),
            answers = receive => ToolExecution::text(serde_json::json!({
                "answers": answers.expect("pending question retains its sender until answered")
            }).to_string()),
        };
        self.waiting.lock().expect("question lock poisoned").take();
        on_event(AgentEvent::ControlChanged);
        result
    }
}

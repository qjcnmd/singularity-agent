//! 当前回合中等待用户回答的问题。等待只在内存中；问题和答案作为工具调用和工具结果写进日志。

use std::sync::Mutex;

use protocol::{PendingQuestion, UserQuestionAnswer};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::AgentEvent;
use crate::tools::{ABORTED_MESSAGE, ToolExecution, error_result};

struct WaitingQuestion {
    request: PendingQuestion,
    reply: oneshot::Sender<Vec<UserQuestionAnswer>>,
}

/// 一次最多等待一组问题；提问工具单独执行，不与其他工具并行。
#[derive(Default)]
pub struct UserQuestions {
    waiting: Mutex<Option<WaitingQuestion>>,
}

impl std::fmt::Debug for UserQuestions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("UserQuestions");
        if let Ok(slot) = self.waiting.try_lock() {
            debug.field("pending_item_id", &slot.as_ref().map(|waiting| waiting.request.item_id.as_str()));
        }
        debug.finish_non_exhaustive()
    }
}

struct ClearGuard<'a> {
    waiting: &'a Mutex<Option<WaitingQuestion>>,
}

impl Drop for ClearGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.waiting.lock() {
            slot.take();
        }
    }
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

    /// 检查界面提交的答案，通过后交给等待中的工具；不通过时问题保留，用户可以修改后重交。
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
            let answer =
                answers.iter().find(|answer| answer.id == question.id).ok_or("答案与问题不匹配。")?;
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
        let clear = ClearGuard { waiting: &self.waiting };
        on_event(AgentEvent::SessionStateChanged);
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => error_result(ABORTED_MESSAGE),
            answers = receive => match answers {
                Ok(answers) => ToolExecution::text(serde_json::json!({ "answers": answers }).to_string()),
                Err(_) => error_result(ABORTED_MESSAGE),
            },
        };
        drop(clear);
        on_event(AgentEvent::SessionStateChanged);
        result
    }
}

use super::*;

impl Conversation {
    /// 所有回合共用同一份后台进程；取消前台不取消这些进程。
    pub fn bash_tasks(&self) -> &Arc<singularity_agent::tools::BashTasks> {
        &self.bash_tasks
    }

    /// 保存的后台任务仅用于展示；当前进程的实时输出覆盖相同身份的历史记录。
    pub fn background_tasks(&self) -> Vec<singularity_protocol::BackgroundTask> {
        let mut tasks = std::collections::BTreeMap::new();
        for entry in lock_writer(&self.writer).entries() {
            if let singularity_agent::session::SessionEntry::Record {
                record: singularity_agent::session::LedgerRecord::BackgroundTaskUpdated { task },
                ..
            } = entry
            {
                let mut task = task.clone();
                if task.state.is_running() {
                    task.state = singularity_protocol::BackgroundTaskState::Interrupted;
                }
                tasks.insert(task.task_id.clone(), task);
            }
        }
        for task in self.bash_tasks.list() {
            tasks.insert(task.task_id.clone(), task);
        }
        tasks.into_values().collect()
    }

    /// 直接累计已保存的用量，不为此复制历史或建立分页索引。
    pub fn usage(&self) -> singularity_protocol::SessionModelUsage {
        crate::history::session_usage(lock_writer(&self.writer).entries())
    }

    /// 宿主先检查是否有可继续的输入，再读取历史，避免无事可做时打开日志。
    pub fn has_background_resume(&self) -> bool {
        let state = self.lock_state();
        !state.turn.is_busy() && state.has_pending_resume()
    }

    /// 空闲时把已到达的通知一次交给同一个普通回合。宿主与结算共用 slot 锁。
    pub fn reserve_background_turn(self: &Arc<Self>) -> Option<OperationReservation> {
        let mut state = self.lock_state();
        if state.turn.is_busy() || !state.has_pending_resume() {
            return None;
        }
        state.turn = TurnLifecycle::Reserved;
        Some(OperationReservation {
            conversation: Arc::clone(self),
            operation: ReservedOperation::Turn,
        })
    }

    /// 存储失败保留待处理事实，并暂停自动执行；用户下一条输入才重新开放。
    pub fn pause_background_resume(&self) {
        let mut state = self.lock_state();
        state.auto_resume = false;
        if let TurnLifecycle::Running(controls) = &state.turn {
            controls.cancellation().cancel();
        }
    }

    /// 归档或退出先关闭自动启动入口，再等待后台保存、发布最终结果。
    pub fn set_closing(&self, closing: bool) {
        self.lock_state().closing = closing;
    }
}

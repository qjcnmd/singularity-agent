//! Thread, turn, item, and conversation-history operations.

use super::checkpoint_recovery::OwnerlessTerminalization;
use super::support::*;
use super::*;

/// 按模型重建所需顺序排列的一页已完成对话历史。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadHistoryPage {
    /// 按 turn 顺序排列的 conversation message。
    pub messages: Vec<ConversationMessage>,
    /// 下一页查询使用的 exclusive turn sequence。
    pub next_before_turn_sequence: Option<u64>,
}

/// 创建 turn、用户条目、追踪和初始历史页后得到的原子结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StartedTurn {
    /// 新建的 turn。
    pub turn: Turn,
    /// 清理后的 user input item。
    pub item: Item,
    /// 与创建操作绑定的 trace event。
    pub trace: TraceEvent,
    /// 创建边界之前可供模型重建的历史页。
    pub history: ThreadHistoryPage,
}

/// 尚未持久化、仅用于在 turn 初始化前绑定进程内资源的唯一标识。
#[derive(Debug)]
pub struct AllocatedTurnId(String);

impl AllocatedTurnId {
    /// 返回将由 Store 原子持久化的 turn id。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// AppServer 在执行 AgentLoop 前预分配、并由终态事务消费的 assistant item ID。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatedAssistantItemId(String);

impl AllocatedAssistantItemId {
    /// 返回预分配 ID 的字符串视图。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 使用预分配 id 创建 started turn 事务所需的字段。
pub struct CreateStartedTurnParams<'a> {
    /// 所属 thread。
    pub thread_id: &'a str,
    /// 初始 AgentLoop 状态。
    pub agent_loop_status: &'a str,
    /// 未清理的用户输入。
    pub input: Value,
    /// started trace 的组件名。
    pub component: &'a str,
    /// started trace 的摘要。
    pub summary: &'a str,
    /// 创建边界需要读取的历史 turn 上限。
    pub history_turn_limit: usize,
}

/// turn 的原子结果，以及助手条目和追踪（如有）。
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedTurnOutcome {
    /// 提交后的 turn 状态。
    pub turn: Turn,
    /// 可选的持久化 assistant item。
    pub assistant_item: Option<Item>,
    /// 与结果提交绑定的 trace event。
    pub trace: TraceEvent,
}

/// 终态提交时拥有该状态归约权的 typed 原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcomeAuthority {
    /// 普通 AgentLoop/用户路径，必须遵守 cancel-requested 的取消合同。
    AgentLoop,
    /// 监视基础设施故障，唯一允许的归约是 Failed，不伪装成 Interrupted。
    InfrastructureFailure,
}

/// 为一个终止、已中断或 approval 阻塞的 turn 结果提交的持久化字段。
pub struct CommitTurnOutcomeParams<'a> {
    /// turn 的目标终态。
    pub status: TurnStatus,
    /// AgentLoop 的目标状态。
    pub agent_loop_status: &'a str,
    /// 与 assistant 增量成对提供的预分配 item ID。
    pub assistant_item_id: Option<&'a AllocatedAssistantItemId>,
    /// 可选的 assistant 增量。
    pub assistant_delta: Option<&'a str>,
    /// 与提交绑定的 trace event。
    pub trace: &'a TraceEvent,
}

impl SessionStore {
    pub fn create_thread(&self, model: Option<&str>, cwd: Option<&str>) -> StoreResult<Thread> {
        let thread = Self::new_thread(model, cwd);
        Self::insert_thread(&self.connection, &thread)?;
        Ok(thread)
    }

    /// 按持久化顺序列出所有 thread。
    pub fn list_threads(&self) -> StoreResult<Vec<Thread>> {
        let mut statement = self.connection.prepare(
            "select thread_id, model, cwd, status
                 from threads order by rowid",
        )?;
        let rows = statement.query_map([], |row| self.thread_from_row(row))?;
        let mut threads = Vec::new();
        for row in rows {
            threads.push(row?);
        }
        Ok(threads)
    }

    /// 读取指定 thread，不存在时返回 NotFound。
    pub fn get_thread(&self, thread_id: &str) -> StoreResult<Thread> {
        self.connection
            .query_row(
                "select thread_id, model, cwd, status
                 from threads where thread_id = ?1",
                params![thread_id],
                |row| self.thread_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })
    }

    /// 原子更新 thread 状态，并在归档前检查非终态 turn。
    pub fn update_thread_status(
        &self,
        thread_id: &str,
        status: ThreadStatus,
    ) -> StoreResult<Thread> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if status == ThreadStatus::Archived {
            Self::ensure_thread_has_no_nonterminal_turn(&transaction, thread_id)?;
        }
        let changed = transaction.execute(
            "update threads set status = ?1 where thread_id = ?2",
            params![status.to_db_text(), thread_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        transaction.commit()?;
        self.get_thread(thread_id)
    }

    /// 删除 thread 及其绑定的 turn、item、trace 和 artifact。
    pub fn delete_thread(&self, thread_id: &str) -> StoreResult<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::ensure_thread_has_no_nonterminal_turn(&transaction, thread_id)?;

        // 按 child-first 顺序删除所有恢复子表。每次删除都核对
        // 事务快照中的行数，避免关系投影不完整时静默留下孤儿数据。
        let expected_turn_inputs: i64 = transaction.query_row(
            "select count(*) from turn_inputs where turn_id in
                 (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
            |row| row.get(0),
        )?;
        let deleted_turn_inputs = transaction.execute(
            "delete from turn_inputs where turn_id in
                 (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
        )? as i64;
        if deleted_turn_inputs != expected_turn_inputs {
            return Err(StoreError::InvalidState(
                "thread deletion changed turn input rows".to_string(),
            ));
        }

        let expected_tool_executions: i64 = transaction.query_row(
            "select count(*) from tool_executions where thread_id = ?1
             or turn_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
            |row| row.get(0),
        )?;
        let deleted_tool_executions = transaction.execute(
            "delete from tool_executions where thread_id = ?1
             or turn_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
        )? as i64;
        if deleted_tool_executions != expected_tool_executions {
            return Err(StoreError::InvalidState(
                "thread deletion changed tool execution rows".to_string(),
            ));
        }

        let expected_turn_checkpoints: i64 = transaction.query_row(
            "select count(*) from turn_checkpoints where thread_id = ?1
             or turn_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
            |row| row.get(0),
        )?;
        let deleted_turn_checkpoints = transaction.execute(
            "delete from turn_checkpoints where thread_id = ?1
             or turn_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
        )? as i64;
        if deleted_turn_checkpoints != expected_turn_checkpoints {
            return Err(StoreError::InvalidState(
                "thread deletion changed turn checkpoint rows".to_string(),
            ));
        }

        let artifact_ids = {
            let mut statement = transaction.prepare(
                "select artifact_id from artifact_refs where run_id = ?1
                 or item_id in (select item_id from items where turn_id in
                     (select turn_id from turns where thread_id = ?1))",
            )?;
            statement
                .query_map(params![thread_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };

        let expected_items: i64 = transaction.query_row(
            "select count(*) from items where turn_id in
                 (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
            |row| row.get(0),
        )?;
        let deleted_items = transaction.execute(
            "delete from items where turn_id in
                 (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
        )? as i64;
        if deleted_items != expected_items {
            return Err(StoreError::InvalidState(
                "thread deletion changed item rows".to_string(),
            ));
        }

        let expected_trace_events: i64 = transaction.query_row(
            "select count(*) from trace_events where run_id = ?1
             or session_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
            |row| row.get(0),
        )?;
        let deleted_trace_events = transaction.execute(
            "delete from trace_events where run_id = ?1
             or session_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
        )? as i64;
        if deleted_trace_events != expected_trace_events {
            return Err(StoreError::InvalidState(
                "thread deletion changed trace rows".to_string(),
            ));
        }

        let expected_artifact_refs = artifact_ids.len() as i64;
        let mut deleted_artifact_refs = 0_i64;
        for artifact_id in &artifact_ids {
            deleted_artifact_refs += transaction.execute(
                "delete from artifact_refs where artifact_id = ?1",
                params![artifact_id],
            )? as i64;
        }
        if deleted_artifact_refs != expected_artifact_refs {
            return Err(StoreError::InvalidState(
                "thread deletion changed artifact rows".to_string(),
            ));
        }

        let expected_turns: i64 = transaction.query_row(
            "select count(*) from turns where thread_id = ?1",
            params![thread_id],
            |row| row.get(0),
        )?;
        let deleted_turns = transaction
            .execute("delete from turns where thread_id = ?1", params![thread_id])?
            as i64;
        if deleted_turns != expected_turns {
            return Err(StoreError::InvalidState(
                "thread deletion changed turn rows".to_string(),
            ));
        }
        let changed = transaction.execute(
            "delete from threads where thread_id = ?1",
            params![thread_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        transaction.commit()?;
        Ok(())
    }

    /// 原子创建 thread 及其初始 trace。
    pub fn create_thread_with_trace(
        &self,
        model: Option<&str>,
        cwd: Option<&str>,
        component: &str,
        summary: &str,
    ) -> StoreResult<(Thread, TraceEvent)> {
        let transaction = self.connection.unchecked_transaction()?;
        let thread = Self::new_thread(model, cwd);
        Self::insert_thread(&transaction, &thread)?;
        let trace = TraceEvent::new(
            format!("trace_{}", thread.thread_id),
            thread.thread_id.clone(),
            thread.thread_id.clone(),
            component,
            summary,
        );
        let trace = Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok((thread, trace))
    }

    /// 创建一个受 thread 与 workspace 并发约束的 turn。
    pub fn create_turn(&self, thread_id: &str, agent_loop_status: &str) -> StoreResult<Turn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::ensure_active_thread(&transaction, thread_id)?;
        Self::ensure_workspace_has_no_nonterminal_turn(&transaction, thread_id, None)?;
        let turn_sequence = Self::next_turn_sequence(&transaction, thread_id)?;
        let turn = Self::new_turn(thread_id, agent_loop_status);
        Self::insert_turn(&transaction, &turn, turn_sequence)?;
        let trace = typed_turn_start_trace(
            format!("trace_{}", turn.turn_id),
            thread_id,
            &turn.turn_id,
            "store",
            "turn started",
        );
        Self::insert_turn_trace(&transaction, &trace, thread_id, &turn.turn_id)?;
        transaction.commit()?;
        Ok(turn)
    }

    /// 创建 turn、user input 和 trace，不返回历史页。
    pub fn create_turn_with_input_and_trace(
        &self,
        thread_id: &str,
        agent_loop_status: &str,
        input: Value,
        component: &str,
        summary: &str,
    ) -> StoreResult<(Turn, Item, TraceEvent)> {
        let started = self.create_turn_with_input_trace_and_history(
            thread_id,
            agent_loop_status,
            input,
            component,
            summary,
            0,
        )?;
        Ok((started.turn, started.item, started.trace))
    }

    /// 原子地创建 turn、清理其输入、记录追踪，并读取此前的历史。
    pub fn create_turn_with_input_trace_and_history(
        &self,
        thread_id: &str,
        agent_loop_status: &str,
        input: Value,
        component: &str,
        summary: &str,
        history_turn_limit: usize,
    ) -> StoreResult<StartedTurn> {
        self.create_allocated_turn_with_input_trace_and_history(
            Self::allocate_turn_id(),
            CreateStartedTurnParams {
                thread_id,
                agent_loop_status,
                input,
                component,
                summary,
                history_turn_limit,
            },
        )
    }

    /// 分配一个尚未持久化的 turn id，供调用方先建立所有易失败的运行资源。
    pub fn allocate_turn_id() -> AllocatedTurnId {
        AllocatedTurnId(format!("turn_{}", short_id()))
    }

    /// 在 AgentLoop 执行前分配终态 assistant item 将使用的稳定 ID。
    pub fn allocate_assistant_item_id() -> AllocatedAssistantItemId {
        AllocatedAssistantItemId(format!("item_{}", short_id()))
    }

    /// 使用预分配 id 原子创建 turn、输入、trace 和此前历史。
    pub fn create_allocated_turn_with_input_trace_and_history(
        &self,
        allocated_turn_id: AllocatedTurnId,
        params: CreateStartedTurnParams<'_>,
    ) -> StoreResult<StartedTurn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::ensure_active_thread(&transaction, params.thread_id)?;
        Self::ensure_workspace_has_no_nonterminal_turn(&transaction, params.thread_id, None)?;
        let history = Self::read_thread_history_from(
            &transaction,
            params.thread_id,
            None,
            params.history_turn_limit,
        )?;
        let turn_sequence = Self::next_turn_sequence(&transaction, params.thread_id)?;
        let turn = Self::new_turn_with_id(
            allocated_turn_id.0,
            params.thread_id,
            params.agent_loop_status,
        );
        Self::insert_turn(&transaction, &turn, turn_sequence)?;
        let item_sequence = Self::next_item_sequence(&transaction, &turn.turn_id)?;
        let (input, redacted) = sanitize_item_payload(&ItemKind::UserMessage, params.input)?;
        let item = Self::new_item(&turn.turn_id, ItemKind::UserMessage, input);
        Self::insert_item(&transaction, &item, item_sequence, redacted)?;
        let trace = typed_turn_start_trace(
            format!("trace_{}", turn.turn_id),
            params.thread_id,
            &turn.turn_id,
            params.component,
            params.summary,
        );
        let trace = Self::insert_turn_trace(&transaction, &trace, params.thread_id, &turn.turn_id)?;
        transaction.commit()?;
        Ok(StartedTurn {
            turn,
            item,
            trace,
            history,
        })
    }

    /// 读取指定 thread 的 completed conversation 历史页。
    pub fn read_thread_history(
        &self,
        thread_id: &str,
        before_turn_sequence: Option<u64>,
        turn_limit: usize,
    ) -> StoreResult<ThreadHistoryPage> {
        let transaction = self.connection.unchecked_transaction()?;
        if !Self::exists_in_transaction(
            &transaction,
            "select 1 from threads where thread_id = ?1",
            thread_id,
        )? {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        let history = Self::read_thread_history_from(
            &transaction,
            thread_id,
            before_turn_sequence,
            turn_limit,
        )?;
        transaction.commit()?;
        Ok(history)
    }

    /// 返回该 thread 最近一个 completed turn 的 turn_id（跨轮 seed 选择入口）。
    pub fn latest_completed_turn_id(&self, thread_id: &str) -> StoreResult<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "select turn_id from turns where thread_id = ?1 and status = ?2
                 order by turn_sequence desc limit 1",
                params![thread_id, TurnStatus::Completed.to_db_text()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// 在指定 turn sequence 之前读取一页历史。
    pub fn read_thread_history_before_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        turn_limit: usize,
    ) -> StoreResult<ThreadHistoryPage> {
        let transaction = self.connection.unchecked_transaction()?;
        let turn_sequence = transaction
            .query_row(
                "select turn_sequence from turns where turn_id = ?1 and thread_id = ?2",
                params![turn_id, thread_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id} in thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let history = Self::read_thread_history_from(
            &transaction,
            thread_id,
            Some(sequence_from_sql(turn_sequence, "turn sequence")?),
            turn_limit,
        )?;
        transaction.commit()?;
        Ok(history)
    }

    /// 读取指定 turn，不存在时返回 NotFound。
    pub fn get_turn(&self, turn_id: &str) -> StoreResult<Turn> {
        self.connection
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(format!("turn {turn_id}")),
                other => StoreError::Sqlite(other),
            })
    }

    /// 更新 turn status，并拒绝覆盖已终态的 turn。
    pub fn update_turn_status(&self, turn_id: &str, status: TurnStatus) -> StoreResult<Turn> {
        self.ensure_turn_status_update_allowed(turn_id, &status, None)?;
        let changed = self.connection.execute(
            "update turns set status = ?1 where turn_id = ?2",
            params![status.to_db_text(), turn_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    /// 在不产生附加 item/trace 的情况下更新 turn 与 AgentLoop 状态。
    pub fn update_turn_state(
        &self,
        turn_id: &str,
        status: TurnStatus,
        agent_loop_status: &str,
    ) -> StoreResult<Turn> {
        self.ensure_turn_status_update_allowed(turn_id, &status, Some(agent_loop_status))?;
        let changed = self.connection.execute(
            "update turns set status = ?1, agent_loop_status = ?2 where turn_id = ?3",
            params![status.to_db_text(), agent_loop_status, turn_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    /// 在一个事务中提交 turn 状态及其持久化条目和追踪。
    pub fn commit_turn_outcome(
        &self,
        turn_id: &str,
        params: CommitTurnOutcomeParams<'_>,
    ) -> StoreResult<CommittedTurnOutcome> {
        self.commit_turn_outcome_with_authority(turn_id, params, TurnOutcomeAuthority::AgentLoop)
    }

    /// 在 typed 基础设施故障权限下提交 turn 终态。
    pub fn commit_turn_outcome_with_authority(
        &self,
        turn_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        authority: TurnOutcomeAuthority,
    ) -> StoreResult<CommittedTurnOutcome> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let committed =
            self.commit_turn_outcome_in_transaction(&transaction, turn_id, params, authority)?;
        transaction.commit()?;
        Ok(committed)
    }

    // 在既有事务中写入 turn 终态、附加 items 和 trace。
    pub(crate) fn commit_turn_outcome_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        turn_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        authority: TurnOutcomeAuthority,
    ) -> StoreResult<CommittedTurnOutcome> {
        let CommitTurnOutcomeParams {
            status,
            agent_loop_status,
            assistant_item_id,
            assistant_delta,
            trace,
        } = params;
        let current = transaction
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        validate_turn_status_update(&current, &status, Some(agent_loop_status), authority)?;
        validate_turn_trace_binding(trace, &current.thread_id, &current.turn_id)?;
        if is_terminal_turn_status(&status) {
            let boundary_pending: bool = transaction.query_row(
                "select pause_requested = 1 or exists(
                    select 1 from turn_inputs
                    where turn_id = ?1 and delivery_state = 'pending'
                 ) from turns where turn_id = ?1",
                params![turn_id],
                |row| row.get(0),
            )?;
            if boundary_pending {
                return Err(StoreError::TurnBoundaryPending {
                    turn_id: turn_id.to_string(),
                });
            }

            let running_execution_count: i64 = transaction.query_row(
                "select count(*) from tool_executions
                 where turn_id = ?1 and execution_state = 'running'",
                params![turn_id],
                |row| row.get(0),
            )?;
            if running_execution_count != 0 {
                if status == TurnStatus::Completed {
                    return Err(StoreError::InvalidState(
                        "completed turn outcome cannot commit with running tool execution"
                            .to_string(),
                    ));
                }
                transaction.execute(
                    "update tool_executions set execution_state = 'unknown'
                     where turn_id = ?1 and execution_state = 'running'",
                    params![turn_id],
                )?;
            }
        }
        match (&status, assistant_item_id, assistant_delta) {
            (TurnStatus::Completed, Some(item_id), Some(delta))
                if !item_id.as_str().trim().is_empty() && !delta.trim().is_empty() => {}
            (TurnStatus::Completed, _, _) => {
                return Err(StoreError::InvalidState(
                    "completed turn outcome requires a preallocated item ID and non-empty assistant message"
                        .to_string(),
                ));
            }
            (_, None, None) => {}
            (_, _, _) => {
                return Err(StoreError::InvalidState(
                    "only a completed turn outcome may include a paired assistant item ID and message"
                        .to_string(),
                ));
            }
        }
        let trace = if is_terminal_turn_status(&status) {
            typed_turn_end_trace(transaction, trace, &current, &status)?
        } else {
            trace.clone()
        };

        transaction.execute(
            "update turns set status = ?1, agent_loop_status = ?2 where turn_id = ?3",
            params![status.to_db_text(), agent_loop_status, turn_id],
        )?;
        let trace_thread_id = current.thread_id.clone();
        let turn = Turn {
            status,
            agent_loop_status: agent_loop_status.to_string(),
            ..current
        };
        let assistant_item = assistant_item_id
            .zip(assistant_delta)
            .map(|(item_id, delta)| -> StoreResult<Item> {
                let kind = ItemKind::AgentMessage;
                let (payload, redacted) =
                    sanitize_item_payload(&kind, serde_json::json!({"delta": delta}))?;
                if Self::item_id_exists(transaction, item_id.as_str())? {
                    return Err(StoreError::InvalidState(
                        "preallocated assistant item ID is already in use".to_string(),
                    ));
                }
                let item = Self::new_item_with_id(item_id.as_str(), turn_id, kind, payload);
                let item_sequence = Self::next_item_sequence(transaction, turn_id)?;
                Self::insert_item(transaction, &item, item_sequence, redacted)?;
                Ok(item)
            })
            .transpose()?;
        let trace = Self::insert_turn_trace(transaction, &trace, &trace_thread_id, turn_id)?;
        Ok(CommittedTurnOutcome {
            turn,
            assistant_item,
            trace,
        })
    }

    /// 记录取消；paused/suspended 当场终态化，running 置 cancel_requested。
    pub fn request_turn_cancellation(
        &self,
        turn_id: &str,
        trace: &TraceEvent,
    ) -> StoreResult<Turn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut turn = transaction
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        if is_terminal_turn_status(&turn.status) || turn.agent_loop_status == "cancel_requested" {
            transaction.commit()?;
            return Ok(turn);
        }
        validate_turn_trace_binding(trace, &turn.thread_id, &turn.turn_id)?;
        // paused/suspended 已无 active owner，不会再有 worker 收敛
        // cancel_requested；用户 interrupt 时当场终态化，避免卡死到重启。
        if turn.status == TurnStatus::Paused || turn.status == TurnStatus::Suspended {
            Self::terminalize_ownerless_turn(
                &transaction,
                &turn.thread_id,
                &turn.turn_id,
                OwnerlessTerminalization {
                    previous_status: turn.status.clone(),
                    previous_agent_loop_status: &turn.agent_loop_status,
                    terminal_agent_loop_status: "cancelled",
                    recovery_reason: "execution_owner_lost",
                    trace,
                },
            )?;
            turn.status = TurnStatus::Interrupted;
            turn.agent_loop_status = "cancelled".to_string();
            transaction.commit()?;
            return Ok(turn);
        }
        transaction.execute(
            "update turns set agent_loop_status = 'cancel_requested' where turn_id = ?1",
            params![turn_id],
        )?;
        Self::insert_turn_trace(&transaction, trace, &turn.thread_id, &turn.turn_id)?;
        turn.agent_loop_status = "cancel_requested".to_string();
        transaction.commit()?;
        Ok(turn)
    }

    /// 清理 payload 后向 turn 追加一个 item。
    pub fn append_item(&self, turn_id: &str, kind: ItemKind, payload: Value) -> StoreResult<Item> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if !Self::exists_in_transaction(
            &transaction,
            "select 1 from turns where turn_id = ?1",
            turn_id,
        )? {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        let item_sequence = Self::next_item_sequence(&transaction, turn_id)?;
        let (payload, redacted) = sanitize_item_payload(&kind, payload)?;
        let item = Self::new_item(turn_id, kind, payload);
        Self::insert_item(&transaction, &item, item_sequence, redacted)?;
        transaction.commit()?;
        Ok(item)
    }

    /// 读取 turn 的 user input payload，供 turn/resume 重建上下文。
    pub fn get_turn_user_input(&self, turn_id: &str) -> StoreResult<Value> {
        let payload: String = self
            .connection
            .query_row(
                "select payload from items where turn_id = ?1 and kind = ?2 order by item_sequence limit 1",
                params![turn_id, ItemKind::UserMessage.to_db_text()],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn user input {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        Ok(serde_json::from_str(&payload)?)
    }
}

impl SessionStore {
    // 在给定事务边界内读取并投影 completed conversation history。
    pub(crate) fn read_thread_history_from(
        connection: &Connection,
        thread_id: &str,
        before_turn_sequence: Option<u64>,
        turn_limit: usize,
    ) -> StoreResult<ThreadHistoryPage> {
        if turn_limit == 0 {
            return Ok(ThreadHistoryPage {
                messages: Vec::new(),
                next_before_turn_sequence: None,
            });
        }

        struct DecodedTurn {
            turn_id: String,
            sequence: u64,
            status: TurnStatus,
            messages: Vec<ConversationMessage>,
        }

        type RawHistoryRow = (
            String,
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
        );

        let mut cursor = before_turn_sequence
            .map(|sequence| sequence_to_sql(sequence, "before turn sequence"))
            .transpose()?;
        let batch_size = turn_limit
            .saturating_add(1)
            .clamp(HISTORY_SCAN_BATCH_TURNS, 256);
        let batch_size = i64::try_from(batch_size).expect("bounded history scan size");
        let mut eligible = Vec::with_capacity(turn_limit.saturating_add(1));
        let mut exhausted = false;

        while eligible.len() <= turn_limit && !exhausted {
            let mut statement = connection.prepare(
                "with selected_turns as (
                     select turn_id, turn_sequence, status
                     from turns
                     where thread_id = ?1
                       and (?2 is null or turn_sequence < ?2)
                     order by turn_sequence desc
                     limit ?3
                 )
                 select t.turn_id, t.turn_sequence, t.status,
                        i.item_id, i.turn_id, i.item_sequence, i.kind,
                        i.payload, i.status, i.redacted
                 from selected_turns t
                 left join items i on i.turn_id = t.turn_id
                 order by t.turn_sequence desc, i.item_sequence",
            )?;
            let rows = statement.query_map(params![thread_id, cursor, batch_size], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            })?;
            let raw_rows = rows.collect::<Result<Vec<RawHistoryRow>, _>>()?;
            drop(statement);
            if raw_rows.is_empty() {
                break;
            }

            let mut decoded_turns: Vec<DecodedTurn> = Vec::new();
            for (
                turn_id,
                turn_sequence,
                serialized_turn_status,
                item_id,
                item_turn_id,
                item_sequence,
                serialized_kind,
                serialized_payload,
                serialized_item_status,
                redacted,
            ) in raw_rows
            {
                let needs_turn = decoded_turns
                    .last()
                    .is_none_or(|turn| turn.turn_id != turn_id);
                if needs_turn {
                    let status = TurnStatus::from_storage_text(&serialized_turn_status)
                        .ok_or_else(|| unknown_db_enum("turn status", &serialized_turn_status))?;
                    decoded_turns.push(DecodedTurn {
                        turn_id: turn_id.clone(),
                        sequence: sequence_from_sql(turn_sequence, "turn sequence")?,
                        status,
                        messages: Vec::new(),
                    });
                }
                let turn = decoded_turns
                    .last_mut()
                    .expect("history row always creates its turn");

                let (
                    item_id,
                    item_turn_id,
                    item_sequence,
                    serialized_kind,
                    serialized_payload,
                    serialized_item_status,
                    redacted,
                ) = match (
                    item_id,
                    item_turn_id,
                    item_sequence,
                    serialized_kind,
                    serialized_payload,
                    serialized_item_status,
                    redacted,
                ) {
                    (
                        Some(item_id),
                        Some(item_turn_id),
                        Some(item_sequence),
                        Some(serialized_kind),
                        Some(serialized_payload),
                        Some(serialized_item_status),
                        Some(redacted),
                    ) => (
                        item_id,
                        item_turn_id,
                        item_sequence,
                        serialized_kind,
                        serialized_payload,
                        serialized_item_status,
                        redacted,
                    ),
                    (None, None, None, None, None, None, None) => continue,
                    _ => {
                        return Err(StoreError::InvalidState(format!(
                            "turn {turn_id} has a partially null item row"
                        )));
                    }
                };
                if item_turn_id != turn.turn_id {
                    return Err(StoreError::InvalidState(format!(
                        "item {item_id} turn binding does not match selected turn"
                    )));
                }
                let kind = ItemKind::from_storage_text(&serialized_kind)
                    .ok_or_else(|| unknown_db_enum("item kind", &serialized_kind))?;
                let item_status = ItemStatus::from_storage_text(&serialized_item_status)
                    .ok_or_else(|| unknown_db_enum("item status", &serialized_item_status))?;
                let stored_redacted = match redacted {
                    0 => false,
                    1 => true,
                    _ => {
                        return Err(StoreError::InvalidState(format!(
                            "item {item_id} redaction flag is invalid"
                        )));
                    }
                };
                let payload: Value =
                    serde_json::from_str(&serialized_payload).map_err(|error| {
                        StoreError::InvalidState(format!(
                            "item {item_id} payload is invalid: {error}"
                        ))
                    })?;
                let (payload, detected_redaction) = sanitize_item_payload(&kind, payload)?;
                if item_status == ItemStatus::Completed
                    && matches!(kind, ItemKind::UserMessage | ItemKind::AgentMessage)
                {
                    let (role, content) = conversation_projection(&kind, &payload)?;
                    turn.messages.push(ConversationMessage {
                        item_id,
                        turn_id: turn.turn_id.clone(),
                        turn_sequence: turn.sequence,
                        item_sequence: sequence_from_sql(item_sequence, "item sequence")?,
                        role,
                        content,
                        redacted: stored_redacted || detected_redaction,
                    });
                }
            }

            let scanned_turn_count = decoded_turns.len();
            cursor = decoded_turns
                .last()
                .map(|turn| sequence_to_sql(turn.sequence, "history scan cursor"))
                .transpose()?;
            for turn in decoded_turns {
                // Issue #24：放宽为任意长度的 completed conversation。
                // 首条必须为 User、末条必须为 Assistant，公共消息非空且 item
                // sequence 严格递增；items 本身已在上面投影时过滤为已完成
                // User/Assistant 消息。带 steer/follow-up 的轮次不再被排除。
                if turn.status == TurnStatus::Completed
                    && !turn.messages.is_empty()
                    && turn.messages[0].role == ConversationRole::User
                    && turn.messages.last().map(|message| message.role.clone())
                        == Some(ConversationRole::Assistant)
                    && turn
                        .messages
                        .windows(2)
                        .all(|pair| pair[0].item_sequence < pair[1].item_sequence)
                {
                    eligible.push(turn);
                    if eligible.len() > turn_limit {
                        break;
                    }
                }
            }
            exhausted = scanned_turn_count < usize::try_from(batch_size).expect("positive batch");
        }

        let has_more = eligible.len() > turn_limit;
        if has_more {
            eligible.truncate(turn_limit);
        }
        let next_before_turn_sequence = has_more.then(|| {
            eligible
                .last()
                .expect("history page with more rows is non-empty")
                .sequence
        });
        eligible.reverse();
        let messages = eligible
            .into_iter()
            .flat_map(|turn| turn.messages)
            .collect();
        Ok(ThreadHistoryPage {
            messages,
            next_before_turn_sequence,
        })
    }
    // 拒绝向不存在或已归档 thread 创建可执行 turn。
    pub(crate) fn ensure_active_thread(
        connection: &Connection,
        thread_id: &str,
    ) -> StoreResult<()> {
        let status = connection
            .query_row(
                "select status from threads where thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let status = ThreadStatus::from_db_text(&status).ok_or_else(|| {
            StoreError::InvalidState(format!("thread {thread_id} has malformed status"))
        })?;
        if status != ThreadStatus::Active {
            return Err(StoreError::InvalidState(format!(
                "thread {thread_id} is not active"
            )));
        }
        Ok(())
    }

    // 检查 thread 是否存在未终态 turn。
    pub(crate) fn ensure_thread_has_no_nonterminal_turn(
        connection: &Connection,
        thread_id: &str,
    ) -> StoreResult<()> {
        let turn_id = connection
            .query_row(
                "select turn_id from turns
                 where thread_id = ?1 and status not in (?2, ?3, ?4)
                 order by turn_sequence limit 1",
                params![
                    thread_id,
                    TurnStatus::Completed.to_db_text(),
                    TurnStatus::Failed.to_db_text(),
                    TurnStatus::Interrupted.to_db_text(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(turn_id) = turn_id {
            return Err(StoreError::ThreadHasNonterminalTurn {
                thread_id: thread_id.to_string(),
                turn_id,
            });
        }
        Ok(())
    }

    // 检查共享 workspace 是否已被其他 thread 的 turn 占用。
    pub(crate) fn ensure_workspace_has_no_nonterminal_turn(
        connection: &Connection,
        thread_id: &str,
        except_turn_id: Option<&str>,
    ) -> StoreResult<()> {
        let cwd = connection
            .query_row(
                "select cwd from threads where thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let workspace = cwd.as_deref().filter(|cwd| !cwd.trim().is_empty());
        let conflict = connection
            .query_row(
                "select turns.thread_id, turns.turn_id
                 from turns
                 join threads on threads.thread_id = turns.thread_id
                 where ((?1 is not null and threads.cwd = ?1)
                        or (?1 is null and turns.thread_id = ?2))
                   and turns.status not in (?3, ?4, ?5)
                   and (?6 is null or turns.turn_id != ?6)
                 order by turns.rowid
                 limit 1",
                params![
                    workspace,
                    thread_id,
                    TurnStatus::Completed.to_db_text(),
                    TurnStatus::Failed.to_db_text(),
                    TurnStatus::Interrupted.to_db_text(),
                    except_turn_id,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((thread_id, turn_id)) = conflict {
            return Err(StoreError::WorkspaceHasNonterminalTurn { thread_id, turn_id });
        }
        Ok(())
    }

    // 为 thread 分配下一个稳定且单调的 turn sequence。
    pub(crate) fn next_turn_sequence(connection: &Connection, thread_id: &str) -> StoreResult<u64> {
        let current = connection.query_row(
            "select max(turn_sequence) from turns where thread_id = ?1",
            params![thread_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        next_sequence(current, "turn sequence")
    }

    // 为 turn 分配下一个稳定且单调的 item sequence。
    pub(crate) fn next_item_sequence(connection: &Connection, turn_id: &str) -> StoreResult<u64> {
        let current = connection.query_row(
            "select max(item_sequence) from items where turn_id = ?1",
            params![turn_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        next_sequence(current, "item sequence")
    }
    // 将包含持久化列的 threads 行解码为 protocol Thread。
    pub(crate) fn thread_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
        let status: String = row.get(3)?;
        Ok(Thread {
            thread_id: row.get(0)?,
            model: row.get(1)?,
            cwd: row.get(2)?,
            status: decode_db_enum(status, 3)?,
        })
    }

    // 将 turns 行解码为 protocol Turn。
    pub(crate) fn turn_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
        let status: String = row.get(2)?;
        Ok(Turn {
            turn_id: row.get(0)?,
            thread_id: row.get(1)?,
            status: decode_db_enum(status, 2)?,
            agent_loop_status: row.get(3)?,
        })
    }

    // 在调用方事务中读取绑定 turn，避免 claim 后再开启一个不受补偿控制的读取。
    pub(crate) fn turn_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        turn_id: &str,
    ) -> StoreResult<Turn> {
        transaction
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })
    }

    // 校验 turn 状态迁移是否允许覆盖当前状态。
    pub(crate) fn ensure_turn_status_update_allowed(
        &self,
        turn_id: &str,
        next_status: &TurnStatus,
        next_agent_loop_status: Option<&str>,
    ) -> StoreResult<()> {
        let current = self.get_turn(turn_id)?;
        validate_turn_status_update(
            &current,
            next_status,
            next_agent_loop_status,
            TurnOutcomeAuthority::AgentLoop,
        )
    }

    // 构造带新 id、初始 active 状态的 Thread。
    pub(crate) fn new_thread(model: Option<&str>, cwd: Option<&str>) -> Thread {
        Thread {
            thread_id: format!("thread_{}", short_id()),
            model: model.map(str::to_string),
            cwd: cwd.map(str::to_string),
            status: ThreadStatus::Active,
        }
    }

    // 构造绑定 thread 的 running Turn。
    pub(crate) fn new_turn(thread_id: &str, agent_loop_status: &str) -> Turn {
        Self::new_turn_with_id(Self::allocate_turn_id().0, thread_id, agent_loop_status)
    }

    pub(crate) fn new_turn_with_id(
        turn_id: String,
        thread_id: &str,
        agent_loop_status: &str,
    ) -> Turn {
        Turn {
            turn_id,
            thread_id: thread_id.to_string(),
            status: TurnStatus::Running,
            agent_loop_status: agent_loop_status.to_string(),
        }
    }

    // 构造绑定 turn 的 pending Item。
    pub(crate) fn new_item(turn_id: &str, kind: ItemKind, payload: Value) -> Item {
        Self::new_item_with_id(&format!("item_{}", short_id()), turn_id, kind, payload)
    }

    // 使用调用方已验证的稳定 ID 构造绑定 turn 的 Item。
    fn new_item_with_id(item_id: &str, turn_id: &str, kind: ItemKind, payload: Value) -> Item {
        Item {
            item_id: item_id.to_string(),
            turn_id: turn_id.to_string(),
            kind,
            payload,
            status: ItemStatus::Completed,
        }
    }

    // 在当前终态事务内拒绝复用预分配 assistant item ID。
    fn item_id_exists(transaction: &Transaction<'_>, item_id: &str) -> StoreResult<bool> {
        transaction
            .query_row(
                "select exists(select 1 from items where item_id = ?1)",
                params![item_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    // 将 Thread 编码后写入 threads 表。
    pub(crate) fn insert_thread(connection: &Connection, thread: &Thread) -> StoreResult<()> {
        connection.execute(
            "insert into threads(
                thread_id, model, cwd, status
            ) values(?1, ?2, ?3, ?4)",
            params![
                thread.thread_id,
                thread.model,
                thread.cwd,
                thread.status.to_db_text(),
            ],
        )?;
        Ok(())
    }

    // 将 Turn 与显式 sequence 写入 turns 表。
    pub(crate) fn insert_turn(
        connection: &Connection,
        turn: &Turn,
        turn_sequence: u64,
    ) -> StoreResult<()> {
        connection.execute(
            "insert into turns(turn_id, thread_id, turn_sequence, status, agent_loop_status) values(?1, ?2, ?3, ?4, ?5)",
            params![
                turn.turn_id,
                turn.thread_id,
                sequence_to_sql(turn_sequence, "turn sequence")?,
                turn.status.to_db_text(),
                turn.agent_loop_status
            ],
        )?;
        Ok(())
    }

    // 将已脱敏 Item 与显式 sequence 写入 items 表。
    pub(crate) fn insert_item(
        connection: &Connection,
        item: &Item,
        item_sequence: u64,
        redacted: bool,
    ) -> StoreResult<()> {
        connection.execute(
            "insert into items(item_id, turn_id, item_sequence, kind, payload, status, redacted) values(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                item.item_id,
                item.turn_id,
                sequence_to_sql(item_sequence, "item sequence")?,
                item.kind.to_db_text(),
                serde_json::to_string(&item.payload)?,
                item.status.to_db_text(),
                redacted,
            ],
        )?;
        Ok(())
    }
    // 脱敏、哈希并写入 trace event，返回持久化投影。
    pub(crate) fn insert_trace(
        connection: &Connection,
        event: &TraceEvent,
    ) -> StoreResult<TraceEvent> {
        Self::insert_trace_with_internal_payload(connection, event, None)
    }

    /// Insert a sanitized trace event and, when present, bind one private typed payload to the
    /// same SQLite row. The payload is never returned by public trace reads.
    pub(crate) fn insert_trace_with_internal_payload(
        connection: &Connection,
        event: &TraceEvent,
        internal_payload: Option<&Value>,
    ) -> StoreResult<TraceEvent> {
        let mut event = sanitize_trace_event(event);
        if let Some(internal_payload) = internal_payload {
            event.payload_hash = trace_envelope_hash_with_internal(&event, Some(internal_payload));
        }
        let payload = encode_trace_payload(&event, internal_payload)?;
        connection.execute(
            "insert into trace_events(event_id, run_id, session_id, payload) values(?1, ?2, ?3, ?4)",
            params![
                event.event_id,
                event.run_id,
                event.session_id,
                payload
            ],
        )?;
        Ok(event)
    }

    // 在所有 turn 相关写入前统一检查 thread/turn 身份绑定。
    pub(crate) fn insert_turn_trace(
        connection: &Connection,
        event: &TraceEvent,
        thread_id: &str,
        turn_id: &str,
    ) -> StoreResult<TraceEvent> {
        validate_turn_trace_binding(event, thread_id, turn_id)?;
        Self::insert_trace(connection, event)
    }

    // 在调用方事务中执行单参数存在性查询。
    pub(crate) fn exists_in_transaction(
        connection: &Connection,
        query: &str,
        value: &str,
    ) -> StoreResult<bool> {
        let result = connection.query_row(query, params![value], |_| Ok(()));
        match result {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(error) => Err(StoreError::Sqlite(error)),
        }
    }
}

fn typed_turn_start_trace(
    event_id: impl Into<String>,
    thread_id: &str,
    turn_id: &str,
    component: &str,
    summary: &str,
) -> TraceEvent {
    let mut event = TraceEvent::for_turn(event_id, thread_id, turn_id, component, summary);
    event.timestamp = Some(Timestamp::now_utc().to_string());
    event.span_id = Some(format!("turn_span_{turn_id}"));
    event.span_kind = Some(TraceSpanKind::Turn);
    event.span_phase = Some(TraceSpanPhase::Start);
    event
}

pub(crate) fn typed_turn_end_trace(
    transaction: &Transaction<'_>,
    event: &TraceEvent,
    current: &Turn,
    status: &TurnStatus,
) -> StoreResult<TraceEvent> {
    let start = find_trace_span_start(
        transaction,
        &current.thread_id,
        &current.turn_id,
        TraceSpanKind::Turn,
    )?
    .ok_or_else(|| {
        StoreError::InvalidState(format!(
            "turn {} is missing its persisted typed start",
            current.turn_id
        ))
    })?;
    let start_timestamp = start
        .timestamp
        .as_deref()
        .and_then(|timestamp| Timestamp::parse(timestamp).ok())
        .ok_or_else(|| {
            StoreError::InvalidState(format!(
                "turn {} typed start has no valid timestamp",
                current.turn_id
            ))
        })?;
    let end_timestamp = Timestamp::now_utc();
    let start_ms = start_timestamp.unix_ms();
    let end_ms = end_timestamp.unix_ms();
    let duration_ms = end_ms.checked_sub(start_ms).ok_or_else(|| {
        StoreError::InvalidState(format!(
            "turn {} typed end timestamp precedes its persisted start",
            current.turn_id
        ))
    })?;
    let span_status = match status {
        TurnStatus::Completed => TraceSpanStatus::Ok,
        TurnStatus::Failed => TraceSpanStatus::Error,
        TurnStatus::Interrupted => TraceSpanStatus::Cancelled,
        TurnStatus::Running | TurnStatus::Paused | TurnStatus::Suspended | TurnStatus::Blocked => {
            return Err(StoreError::InvalidState(
                "non-terminal turn cannot produce a typed end".to_string(),
            ));
        }
    };
    let mut event = event.clone();
    event.timestamp = Some(end_timestamp.to_string());
    event.span_id = start.span_id;
    event.parent_span_id = None;
    event.span_kind = Some(TraceSpanKind::Turn);
    event.span_phase = Some(TraceSpanPhase::End);
    event.span_status = Some(span_status);
    event.duration_ms = Some(duration_ms);
    event.time_to_first_token_ms = None;
    event.span_projection = None;
    event.metric_samples.clear();
    Ok(event)
}

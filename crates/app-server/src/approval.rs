//! Approval request, center, and decision handling.
//!
//! Phase 3a 起新核心无审批链（approval/requested 事件不再产生）；本模块只保留
//! 协议方法入口与响应形状：approval/decision 仅经 store 记录，不续行执行。

use super::*;

impl AppServer {
    pub(super) fn approval_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let approvals = self.store.list_pending_approvals()?;
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ApprovalListResult { approvals })?,
            )
            .to_wire_value(),
        ])
    }

    pub(super) fn approval_center(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        json_response(
            message.required_id(),
            ApprovalCenterResult {
                pending_approvals: self.store.list_pending_approvals()?,
                decisions: self.store.list_approval_decisions()?,
            },
        )
    }

    pub(super) fn approval_request(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let _request: ApprovalRequest = parse_params(&message)?;
        invalid_state_response(message.required_id(), APPROVAL_REQUEST_INTERNAL_ONLY)
    }

    /// 记录 approval 决定：仅持久化决策，不恢复或执行任何 AgentLoop continuation。
    pub(super) fn approval_decision(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        let result = self.handle_approval_decision_streaming_values(
            message,
            |_| {},
            |message| messages.push(message),
        );
        result?;
        Ok(messages)
    }

    /// 执行 approval/decision 并保留唯一的最终响应（协议形状与 transport 合同不变）。
    pub fn handle_approval_decision_streaming_with_output(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        let coordinator = self.output_order.clone();
        let mut sequencing_error = None;
        let trace_binding = RefCell::new(None);
        let result = self.handle_approval_decision_streaming_values(
            message,
            |binding| *trace_binding.borrow_mut() = Some(binding),
            |message| {
                if sequencing_error.is_some() {
                    return;
                }
                match sequence_output(&coordinator, message, trace_binding.borrow().clone()) {
                    Ok(output) => emit(output),
                    Err(error) => sequencing_error = Some(error),
                }
            },
        );
        if let Some(error) = sequencing_error {
            return Err(error);
        }
        result
    }

    fn handle_approval_decision_streaming_values(
        &mut self,
        message: JsonRpcMessage,
        mut bind_trace: impl FnMut(TransportTraceBinding),
        mut emit: impl FnMut(Value),
    ) -> AppServerResult<()> {
        let decision: ApprovalDecision = parse_params(&message)?;
        let pending_request = match self.store.get_pending_approval(&decision.request_id) {
            Ok(request) => request,
            Err(StoreError::NotFound(_)) => {
                emit_messages(
                    &mut emit,
                    not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND)?,
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        bind_trace(TransportTraceBinding::for_turn(
            pending_request.thread_id.clone(),
            pending_request.turn_id.clone(),
        ));
        // 只记录不续行：决策写入 approvals 表并 resolve pending 行；turn 状态不变。
        match self.store.record_approval_decision(
            &decision,
            "approval",
            "approval decision recorded",
        ) {
            Ok(_recorded) => {}
            Err(error) => {
                let response = match error {
                    StoreError::NotFound(_) => not_found_response(
                        message.required_id(),
                        PENDING_APPROVAL_NOT_FOUND,
                    )?,
                    StoreError::InvalidState(state_message) => {
                        invalid_state_response(message.required_id(), state_message)?
                    }
                    other => return Err(other.into()),
                };
                emit_messages(&mut emit, response);
                return Ok(());
            }
        }
        emit(approval_decision_response(
            message.required_id(),
            &decision,
        )?);
        Ok(())
    }
}

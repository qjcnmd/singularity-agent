use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use singularity_agent::agent::AgentOutcome;
use singularity_core::contains_sensitive_text;
use singularity_protocol::{TraceEvent, TraceSpanKind, TraceSpanPhase};
use singularity_store::{SessionStore, StoreError};

/// 把新核心 `Agent` 运行事件投影到 store 的 trace 流。
///
/// Phase 3b 起只投影新核心能提供的两类事件：工具执行（`on_tool_execution_start`
/// 回调）与 `Agent::run` 终态；旧链 observation 投影路径已删除。
pub struct TraceProjector<'a> {
    store: &'a SessionStore,
    run_id: String,
    session_id: String,
}

impl<'a> TraceProjector<'a> {
    /// Bind the projector to the persisted Turn root via a direct typed Store lookup.
    pub(crate) fn new(
        store: &'a SessionStore,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<Self, StoreError> {
        // 校验 turn 根 span 已持久化；project_tool_execution/project_outcome 不再挂父子 span。
        if store
            .find_span_start(thread_id, turn_id, TraceSpanKind::Turn)?
            .and_then(|event| event.span_id)
            .is_none()
        {
            return Err(StoreError::InvalidState(format!(
                "turn {turn_id} is missing its persisted typed root"
            )));
        }
        Ok(Self {
            store,
            run_id: thread_id.to_string(),
            session_id: turn_id.to_string(),
        })
    }

    /// 投影一次工具执行（`AgentEvents::on_tool_execution_start` 回调，Phase 3a 新核心）。
    ///
    /// 参数原文不落 trace（可能含敏感内容），只投影名称与参数摘要；
    /// 同一 turn 内相同名称+参数只写一条（幂等）。
    pub fn project_tool_execution(&self, tool_name: &str, args: &str) -> Result<(), StoreError> {
        let args_digest = digest_identifier(args);
        let identity = format!("tool_execution:{tool_name}:{args_digest}");
        let mut event = self.new_trace_event(
            trace_event_id(&self.session_id, &identity, TraceSpanPhase::End),
            "tool execution",
        );
        event.payload = json!({
            "observation": "tool_execution",
            "tool_name": tool_name,
            "arguments_digest": args_digest,
        });
        self.store.append_trace_idempotent(&event).map(|_| ())
    }

    /// 投影 `Agent::run` 终态（usage/turns/compaction 等聚合信息，Phase 3a 新核心）。
    pub fn project_outcome(&self, outcome: &AgentOutcome) -> Result<(), StoreError> {
        let status_label = if outcome.aborted {
            "aborted"
        } else {
            "completed"
        };
        let identity = format!("agent_outcome:{}:{status_label}", self.session_id);
        let mut event = self.new_trace_event(
            trace_event_id(&self.session_id, &identity, TraceSpanPhase::End),
            "agent outcome",
        );
        event.payload = json!({
            "observation": "agent_outcome",
            "status": status_label,
            "turns": outcome.turns,
            "compacted": outcome.compacted,
            "aborted": outcome.aborted,
            "usage": {
                "input_tokens": outcome.usage.input_tokens,
                "output_tokens": outcome.usage.output_tokens,
                "total_tokens": outcome.usage.total_tokens,
                "cached_input_tokens": outcome.usage.cached_input_tokens,
                "reasoning_tokens": outcome.usage.reasoning_tokens,
            },
            "final_text": if outcome.final_text.trim().is_empty() {
                Value::Null
            } else {
                json!(redact_app_server_text(&outcome.final_text))
            },
        });
        self.store.append_trace_idempotent(&event).map(|_| ())
    }

    fn new_trace_event(&self, event_id: String, summary: &str) -> TraceEvent {
        TraceEvent::for_turn(
            event_id,
            self.run_id.clone(),
            self.session_id.clone(),
            "observability",
            summary,
        )
    }
}

fn trace_event_id(turn_id: &str, identity: &str, phase: TraceSpanPhase) -> String {
    let material = format!("{turn_id}\u{0}{identity}\u{0}{}", phase.as_storage_text());
    format!("trace_obs_{:x}", Sha256::digest(material.as_bytes()))
}

fn redact_app_server_text(text: &str) -> String {
    if contains_sensitive_text(text) {
        "[redacted sensitive app-server output]".to_string()
    } else {
        text.to_string()
    }
}

fn digest_identifier(value: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}

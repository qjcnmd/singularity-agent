//! ToolBroker 执行边界、决策和公开结果投影。

use super::*;

/// 保留失败类别，使调用方能够区分输入、策略、沙箱和执行错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureKind {
    Input,
    Visibility,
    Capability,
    Policy,
    PermissionProfile,
    WorkspaceBoundary,
    ProtectedPath,
    Approval,
    Sandbox,
    Backend,
    Infrastructure,
    Execution,
    Timeout,
    Cancelled,
}

/// 决定 tool 代理器执行、拒绝还是暂停调用的授权结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolBrokerDecision {
    Allow,
    Approved {
        approval_grant_id: String,
    },
    Deny {
        failure_kind: ToolFailureKind,
        reason: String,
    },
    Ask {
        approval_request_id: String,
        reason: String,
    },
}

impl ToolBrokerDecision {
    /// 构造策略拒绝决策。
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::deny_with_kind(ToolFailureKind::Policy, reason)
    }

    /// 构造带失败类别的拒绝决策。
    pub fn deny_with_kind(failure_kind: ToolFailureKind, reason: impl Into<String>) -> Self {
        Self::Deny {
            failure_kind,
            reason: reason.into(),
        }
    }

    /// 判断决策是否允许进入执行边界。
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow | Self::Approved { .. })
    }

    /// 构造 approval 已授予的决策。
    pub fn approved(approval_grant_id: impl Into<String>) -> Self {
        Self::Approved {
            approval_grant_id: approval_grant_id.into(),
        }
    }

    /// 构造等待 approval 的决策。
    pub fn ask(approval_request_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Ask {
            approval_request_id: approval_request_id.into(),
            reason: reason.into(),
        }
    }
}

/// 执行边界；调用执行器闭包前会重新校验 tool 输入。
#[derive(Debug, Default, Clone)]
pub struct ToolBroker {
    registry: ToolRegistry,
}

impl ToolBroker {
    /// 用已注册的 tool 表创建执行边界。
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry }
    }

    /// 注册 tool 并保持 broker 的名称约束。
    pub fn register(&mut self, entry: ToolEntry) -> Result<(), String> {
        self.registry.register(entry)
    }

    /// 按名称查找 tool 定义。
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.registry.get(name)
    }

    /// 按名称查找完整工具条目。
    pub fn entry(&self, name: &str) -> Option<&ToolEntry> {
        self.registry.entry(name)
    }

    /// 返回满足版本化能力要求的注册条目。
    pub fn entries_for_capability(
        &self,
        capability: ToolCapability,
        minimum_version: u32,
    ) -> Vec<&ToolEntry> {
        self.registry
            .entries_for_capability(capability, minimum_version)
    }

    /// 返回 broker 暴露给模型的 schema。
    pub fn tool_schema_payloads(&self) -> Vec<Value> {
        self.registry.schema_payloads()
    }

    /// 准备模型提交的 tool 输入。
    pub fn prepare_model_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<(ToolExecutionMode, Value), ToolInputValidationError> {
        self.registry.prepare_model_input(name, input)
    }

    /// 由注册表绑定执行器，并由工作区边界投影类型化授权资源。
    pub fn bind_authorization(
        &self,
        name: &str,
        input: Value,
        workspace_tools: Option<&WorkspaceTools>,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
    ) -> Result<BoundToolCall, WorkspaceToolError> {
        let entry = self.registry.entry(name).ok_or_else(|| {
            WorkspaceToolError::InvalidInput("tool is not registered".to_string())
        })?;
        entry
            .validate_consistency()
            .map_err(WorkspaceToolError::InvalidInput)?;
        match entry.executor {
            ToolExecutor::Workspace(executor) => {
                let workspace_tools = workspace_tools.ok_or_else(|| {
                    WorkspaceToolError::InvalidInput(
                        "workspace tool backend is unavailable".to_string(),
                    )
                })?;
                workspace_tools.bind_tool_call(entry, executor, input, filesystem, network)
            }
            ToolExecutor::AgentControl(_) => Ok(BoundToolCall {
                tool_id: entry.id.clone(),
                execution_mode: entry.spec.execution_mode,
                executor: entry.executor,
                operation: PermissionOperation::Read,
                arguments: input,
                resources: vec![PermissionResource::Tool(entry.id.clone())],
                sensitive_resources: BTreeSet::new(),
            }),
        }
    }

    /// 在执行前校验 tool 输入。
    pub fn validate_execution_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<ToolExecutionMode, ToolInputValidationError> {
        self.registry.validate_execution_input(name, input)
    }

    /// 执行允许的调用；否则返回类型化结果且不调用闭包。
    pub fn execute<F>(
        &self,
        envelope: &ToolCallRequest,
        decision: ToolBrokerDecision,
        executor: F,
    ) -> ToolResult
    where
        F: FnOnce(ToolExecutor, &ToolCallRequest) -> ToolOutput,
    {
        let Some(entry) = self.registry.entry(&envelope.tool_name) else {
            return ToolResult::failed_with_kind(
                envelope,
                ToolFailureKind::Visibility,
                UNKNOWN_TOOL_ERROR,
                "tool is not registered",
            );
        };
        if entry.validate_consistency().is_err() {
            return ToolResult::failed_with_kind(
                envelope,
                ToolFailureKind::Infrastructure,
                TOOL_CONTRACT_INVALID_ERROR,
                "tool registration contract is invalid",
            );
        }
        if decision.is_allowed() {
            let input = match serde_json::from_str::<Value>(&envelope.raw_arguments) {
                Ok(input) => input,
                Err(_) => {
                    let output = ToolOutput::failure_with_kind(
                        ToolFailureKind::Input,
                        INVALID_TOOL_ARGUMENTS_ERROR,
                        json!({
                            "summary": "tool arguments failed executable input validation",
                            "validation_code": "invalid_json_arguments",
                        }),
                    );
                    return ToolResult::from_result(envelope, &output);
                }
            };
            if let Err(error) = self
                .registry
                .validate_execution_input(&envelope.tool_name, &input)
            {
                let output = ToolOutput::failure_with_kind(
                    ToolFailureKind::Input,
                    INVALID_TOOL_ARGUMENTS_ERROR,
                    json!({
                        "summary": "tool arguments failed executable input validation",
                        "validation_code": error.code,
                    }),
                );
                return ToolResult::from_result(envelope, &output);
            }
        }
        if let ToolBrokerDecision::Deny {
            failure_kind,
            reason,
        } = decision
        {
            return ToolResult::failed_with_kind(envelope, failure_kind, TOOL_DENIED_ERROR, reason);
        }
        if let ToolBrokerDecision::Ask { reason, .. } = decision {
            return ToolResult::approval_required(envelope, reason);
        }
        ToolResult::from_result(envelope, &executor(entry.executor, envelope))
    }
}

/// 从模型 tool call 传给 tool 代理器和执行器的规范化封装结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
}

impl ToolCallRequest {
    /// 创建规范化的 tool call 封装。
    pub fn new(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        raw_arguments: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            raw_arguments: raw_arguments.into(),
        }
    }
}

/// tool 代理器对其公开投影进行有界化和脱敏前的执行器原始输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutput {
    pub ok: bool,
    pub content: Value,
    pub error_code: Option<String>,
    pub failure_kind: Option<ToolFailureKind>,
    pub truncated: bool,
    pub metadata: Value,
}

impl ToolOutput {
    /// 构造成功输出。
    pub fn success(content: Value) -> Self {
        Self {
            ok: true,
            content,
            error_code: None,
            failure_kind: None,
            truncated: false,
            metadata: json!({}),
        }
    }

    /// 构造默认执行失败输出。
    pub fn failure(error_code: impl Into<String>, content: Value) -> Self {
        Self::failure_with_kind(ToolFailureKind::Execution, error_code, content)
    }

    /// 构造带稳定失败类别的输出。
    pub fn failure_with_kind(
        failure_kind: ToolFailureKind,
        error_code: impl Into<String>,
        content: Value,
    ) -> Self {
        Self {
            ok: false,
            content,
            error_code: Some(error_code.into()),
            failure_kind: Some(failure_kind),
            truncated: false,
            metadata: json!({}),
        }
    }
}

/// 用于派生模型历史、追踪和完成证据的有界内部结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<ToolFailureKind>,
    #[serde(skip)]
    pub result_id: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    context_token_count: Option<u32>,
    pub truncated: bool,
    #[serde(skip)]
    audit_metadata: Option<Value>,
    #[serde(skip)]
    #[schemars(skip)]
    workspace_observation: Option<WorkspaceObservation>,
}

impl ToolResult {
    /// 构造仅含有界预览的结果。
    pub fn summary(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        ok: bool,
        preview: impl Into<String>,
    ) -> Self {
        let mut result = Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            ok,
            content: None,
            preview: Some(redact_public_text(&preview.into())),
            error_code: None,
            failure_kind: None,
            result_id: None,
            context_token_count: None,
            truncated: false,
            audit_metadata: None,
            workspace_observation: None,
        };
        result.context_token_count = Some(approximate_token_count(
            &result.to_message_payload().to_string(),
        ));
        result
    }

    /// 附加内部审计 metadata，不进入模型公开内容。
    pub fn with_audit(mut self, metadata: Value) -> Self {
        self.audit_metadata = Some(metadata);
        self
    }

    /// 返回内部审计 metadata。
    pub fn audit_metadata(&self) -> Option<&Value> {
        self.audit_metadata.as_ref()
    }

    /// 返回由脱敏安全结果派生的内部上下文 accounting，不进入公开 payload。
    pub fn context_token_count(&self) -> Option<u32> {
        self.context_token_count
    }

    /// 从未截断且仍保留完整安全 `content` 的结果重建可信 accounting。
    pub fn reconstruct_context_token_count(&self) -> Option<u32> {
        if self.truncated || self.preview.is_some() {
            return None;
        }
        let content = self.content.as_ref()?;
        let payload = self.to_message_payload();
        if payload.get("content") != Some(content) {
            return None;
        }
        Some(safe_context_token_count(content))
    }

    /// 返回当前安全模型投影可证明的 accounting 下界。
    pub fn context_token_count_lower_bound(&self) -> u32 {
        let payload = self.to_message_payload();
        let accounted_value = payload
            .get("content")
            .or_else(|| payload.get("preview"))
            .unwrap_or(&payload);
        serde_json::to_string(accounted_value)
            .map_or(u32::MAX, |serialized| approximate_token_count(&serialized))
    }

    /// 从 approval checkpoint 恢复内部上下文 accounting。
    pub fn with_context_token_count(mut self, token_count: u32) -> Self {
        self.context_token_count = Some(token_count);
        self
    }

    /// 返回只供 completion/checkpoint 使用的 workspace observation。
    pub fn workspace_observation(&self) -> Option<&WorkspaceObservation> {
        self.workspace_observation.as_ref()
    }

    /// 绑定内部 workspace observation；该字段不会序列化到模型 payload。
    pub fn with_workspace_observation(mut self, observation: WorkspaceObservation) -> Self {
        self.workspace_observation = Some(observation);
        self
    }

    /// 将执行器输出投影为模型和 trace 可用结果。
    pub fn from_result(envelope: &ToolCallRequest, result: &ToolOutput) -> Self {
        let source_truncated = result.truncated
            || result
                .content
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || result
                .content
                .get("output_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let sanitized_content = sanitize_public_value(result.content.clone());
        let context_token_count = safe_context_token_count(&sanitized_content);
        let sanitized_content_chars = sanitized_content.to_string().chars().count();
        let public_content_summarized =
            source_truncated || sanitized_content_chars > DEFAULT_RESULT_PREVIEW_MAX_CHARS;
        let public_content = if public_content_summarized {
            summarize_truncated_value(sanitized_content)
        } else {
            sanitized_content
        };
        let result_content = public_content.to_string();
        let content_is_safe = redact_public_text(&result_content) == result_content;
        let (bounded_preview, preview_truncated) =
            bounded_text(&result_content, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
        let preview = redact_public_text(&bounded_preview);
        let truncated = public_content_summarized || preview_truncated;
        let result_id = result_id(&result.content, &result.metadata);
        let mut tool_result = Self {
            error_code: result.error_code.clone(),
            failure_kind: result.failure_kind.clone(),
            truncated,
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                result.ok,
                preview,
            )
        };
        tool_result.context_token_count = Some(context_token_count);
        if content_is_safe && !source_truncated && !preview_truncated {
            tool_result.content = Some(public_content);
            tool_result.preview = None;
        }
        tool_result.result_id = result_id;
        tool_result.audit_metadata = result.metadata.get("audit").cloned();
        tool_result.workspace_observation = result
            .metadata
            .get(WORKSPACE_OBSERVATION_METADATA)
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        tool_result
    }

    pub fn failed(
        envelope: &ToolCallRequest,
        error_code: impl Into<String>,
        preview: impl Into<String>,
    ) -> Self {
        Self::failed_with_kind(envelope, ToolFailureKind::Execution, error_code, preview)
    }

    pub fn failed_with_kind(
        envelope: &ToolCallRequest,
        failure_kind: ToolFailureKind,
        error_code: impl Into<String>,
        preview: impl Into<String>,
    ) -> Self {
        Self {
            error_code: Some(error_code.into()),
            failure_kind: Some(failure_kind),
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                false,
                preview,
            )
        }
    }

    pub fn approval_required(envelope: &ToolCallRequest, reason: impl Into<String>) -> Self {
        Self::failed_with_kind(
            envelope,
            ToolFailureKind::Approval,
            TOOL_APPROVAL_REQUIRED_ERROR,
            reason,
        )
    }

    pub fn to_message_payload(&self) -> Value {
        let mut payload = json!({
            "ok": self.ok,
            "tool_name": self.tool_name,
            "tool_call_id": self.tool_call_id,
            "truncated": self.truncated,
        });
        if let Some(error_code) = self.error_code.as_deref() {
            payload["error_code"] = json!(error_code);
        }
        if let Some(failure_kind) = self.failure_kind.as_ref() {
            payload["failure_kind"] =
                serde_json::to_value(failure_kind).unwrap_or_else(|_| json!("execution"));
        }
        if let Some(content) = self.content.as_ref() {
            let content = sanitize_public_value(content.clone());
            let serialized = content.to_string();
            let (bounded_preview, content_truncated) =
                bounded_text(&serialized, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
            if !content_truncated && redact_public_text(&serialized) == serialized {
                payload["content"] = content;
            } else {
                payload["preview"] = json!(redact_public_text(&bounded_preview));
                payload["truncated"] = json!(self.truncated || content_truncated);
            }
        } else if let Some(preview) = self.preview.as_deref() {
            let preview = redact_public_text(preview);
            payload["preview"] = json!(preview);
        }
        payload
    }
}

fn result_id(content: &Value, metadata: &Value) -> Option<String> {
    value_string(metadata.get("result_id"))
        .or_else(|| value_string(content.get("result_id")))
        .or_else(|| value_string(metadata.get("output_digest")))
        .or_else(|| value_string(content.get("output_digest")))
}

fn value_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn sanitize_public_value(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(sanitize_public_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .filter_map(|(key, value)| {
                    if is_artifact_reference_key(&key) {
                        None
                    } else {
                        Some((key, sanitize_public_value(value)))
                    }
                })
                .collect(),
        ),
        Value::String(value) if contains_artifact_reference(&value) => {
            Value::String(ARTIFACT_REFERENCE_OMITTED.to_string())
        }
        other => other,
    }
}

fn summarize_truncated_value(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(summarize_truncated_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .filter_map(|(key, value)| {
                    if is_truncated_raw_output_key(&key) {
                        None
                    } else {
                        Some((key, summarize_truncated_value(value)))
                    }
                })
                .collect(),
        ),
        Value::String(value) if value.chars().count() > MAX_TRUNCATED_SUMMARY_STRING_CHARS => {
            Value::String(TRUNCATED_OUTPUT_OMITTED.to_string())
        }
        other => other,
    }
}

fn is_truncated_raw_output_key(key: &str) -> bool {
    TRUNCATED_RAW_OUTPUT_KEYS.contains(&key.to_ascii_lowercase().as_str())
}

fn is_artifact_reference_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "artifact_ref" | "artifact_refs" | "diff_ref" | "diff_refs"
    )
}

pub(crate) fn contains_artifact_reference(value: &str) -> bool {
    value.to_ascii_lowercase().contains("artifact://")
}

/// 只用递归脱敏后的值计算内部 accounting；原始结果不会从该函数返回或持久化。
fn safe_context_token_count(value: &Value) -> u32 {
    let accounting_value = redact_context_value(value.clone());
    serde_json::to_string(&accounting_value)
        .map_or(u32::MAX, |serialized| approximate_token_count(&serialized))
}

/// 为 accounting 递归移除 artifact key，并把敏感文本替换为固定安全摘要。
fn redact_context_value(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(redact_context_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .filter_map(|(key, value)| {
                    if is_artifact_reference_key(&key) {
                        None
                    } else {
                        Some((key, redact_context_value(value)))
                    }
                })
                .collect(),
        ),
        Value::String(value) => Value::String(redact_public_text(&value)),
        other => other,
    }
}

/// 按当前 provider request budget 使用的保守字符规则估算 token 数。
pub fn approximate_token_count(content: &str) -> u32 {
    let mut ascii_chars = 0usize;
    let mut non_ascii_chars = 0usize;
    for character in content.chars() {
        if character.is_ascii() {
            ascii_chars = ascii_chars.saturating_add(1);
        } else {
            non_ascii_chars = non_ascii_chars.saturating_add(1);
        }
    }
    let ascii_tokens = ascii_chars.saturating_add(APPROXIMATE_ASCII_CHARS_PER_TOKEN - 1)
        / APPROXIMATE_ASCII_CHARS_PER_TOKEN;
    let estimated = ascii_tokens.saturating_add(non_ascii_chars);
    u32::try_from(estimated.max(1)).unwrap_or(u32::MAX)
}

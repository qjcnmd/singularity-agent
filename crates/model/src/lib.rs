#![forbid(unsafe_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelPurpose {
    PlanNextAction,
    FinalAnswer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: Vec<ContentBlock>,
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
    pub metadata: Value,
}

impl ModelMessage {
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::text(content)],
            name: None,
            tool_call_id: None,
            metadata: json!({}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContentBlockType {
    Text,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: ContentBlockType,
    pub text: Option<String>,
    pub artifact_ref: Option<String>,
    pub metadata: Value,
}

impl ContentBlock {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            block_type: ContentBlockType::Text,
            text: Some(content.into()),
            artifact_ref: None,
            metadata: json!({}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
    SpecificTool,
    AllowedTools,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolChoicePolicy {
    pub mode: ToolChoiceMode,
    pub tool_name: Option<String>,
    pub allowed_tool_names: Vec<String>,
    pub max_tool_calls: u32,
}

impl Default for ToolChoicePolicy {
    fn default() -> Self {
        Self {
            mode: ToolChoiceMode::Auto,
            tool_name: None,
            allowed_tool_names: Vec::new(),
            max_tool_calls: 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPreferences {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub json_mode: bool,
    pub structured_output_schema: Option<Value>,
    pub stream: bool,
    pub fallback_models: Vec<String>,
}

impl Default for ModelPreferences {
    fn default() -> Self {
        Self {
            provider_name: None,
            model_name: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            json_mode: false,
            structured_output_schema: None,
            stream: false,
            fallback_models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelBudget {
    pub max_input_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub max_total_tokens: Option<u32>,
    pub max_retries: u32,
    pub max_latency_ms: Option<u64>,
    pub max_cost_estimate: Option<f64>,
}

impl Default for ModelBudget {
    fn default() -> Self {
        Self {
            max_input_tokens: None,
            max_output_tokens: None,
            max_total_tokens: None,
            max_retries: 2,
            max_latency_ms: None,
            max_cost_estimate: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_estimate: Option<f64>,
}

impl Default for ModelUsage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cached_input_tokens: 0,
            reasoning_tokens: 0,
            cost_estimate: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub action_id: String,
    pub purpose: ModelPurpose,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<Value>,
    pub tool_choice: ToolChoicePolicy,
    pub model_preferences: ModelPreferences,
    pub budget: ModelBudget,
    pub context_metadata: Value,
    pub policy_metadata: Value,
    pub trace_metadata: Value,
}

impl ModelTurnRequest {
    pub fn new(
        request_id: impl Into<String>,
        run_id: impl Into<String>,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        messages: Vec<ModelMessage>,
    ) -> Self {
        let request_id = request_id.into();
        Self {
            action_id: request_id.clone(),
            request_id,
            run_id: run_id.into(),
            session_id: session_id.into(),
            task_id: task_id.into(),
            phase_id: "model".to_string(),
            purpose: ModelPurpose::PlanNextAction,
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoicePolicy::default(),
            model_preferences: ModelPreferences::default(),
            budget: ModelBudget::default(),
            context_metadata: json!({}),
            policy_metadata: json!({}),
            trace_metadata: json!({}),
        }
    }

    pub fn with_phase_action(
        mut self,
        phase_id: impl Into<String>,
        action_id: impl Into<String>,
    ) -> Self {
        self.phase_id = phase_id.into();
        self.action_id = action_id.into();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnStatus {
    Success,
    Failed,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnResponse {
    pub request_id: String,
    pub response_id: String,
    pub status: ModelTurnStatus,
    pub assistant_message: Option<ModelMessage>,
    pub tool_calls: Vec<Value>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
    pub validation: Option<Value>,
    pub error: Option<Value>,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub latency_ms: Option<u64>,
    pub trace_event_ids: Vec<String>,
    pub raw_response_ref: Option<String>,
    pub metadata: Value,
}

impl ModelTurnResponse {
    pub fn completed(
        request_id: impl Into<String>,
        response_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            response_id: response_id.into(),
            status: ModelTurnStatus::Success,
            assistant_message: Some(ModelMessage::text(ModelRole::Assistant, content)),
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            validation: None,
            error: None,
            provider_name: None,
            model_name: None,
            latency_ms: None,
            trace_event_ids: Vec::new(),
            raw_response_ref: None,
            metadata: json!({}),
        }
    }
}

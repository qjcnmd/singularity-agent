use super::*;
use crate::provider::contract::finalize_provider_response;

/// 读取并解码一次 Chat 流式响应。
pub(crate) async fn read_chat_sse_stream(
    cancellation: &CancellationToken,
    response: reqwest::Response,
    on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    config: &OpenAiProviderConfig,
    selection: &SelectedModel,
    endpoint: &str,
) -> ModelTurnResponse {
    let mut decoder = ChatSseDecoder::new(on_event, config, &selection.model_name, endpoint);
    let result = read_sse_stream(cancellation, response, &mut decoder).await;
    decoder.finish(result)
}

#[derive(Default)]
struct ChatToolAccumulator {
    id: String,
    name: String,
    arguments: String,
}

/// Chat SSE 解码器。正文和思考文本边收边发出；工具参数和提供方私有数据在流结束后
/// 一次性整理进最终回复。
struct ChatSseDecoder<'a> {
    config: &'a OpenAiProviderConfig,
    model_name: &'a str,
    endpoint: &'a str,
    content: String,
    reasoning_content: String,
    reasoning_field: Option<String>,
    reasoning_details: Vec<Value>,
    tool_calls: BTreeMap<u64, ChatToolAccumulator>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    done: bool,
    on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send),
}

impl SseStreamDecoder for ChatSseDecoder<'_> {
    fn frame_malformed() -> fn(&'static str) -> ProviderError {
        provider_chat_stream_malformed_error
    }

    fn dispatch_event(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        let raw = std::str::from_utf8(data)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_utf8"))?
            .trim();
        // [DONE] 表示流结束；读取循环随后停止，之后的数据不再处理。
        if raw == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let payload = serde_json::from_str::<Value>(raw)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_json"))?;
        if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
            return Err(crate::error::provider_embedded_error(
                &crate::error::provider_error_fields(error),
                "provider Chat stream returned an error",
            ));
        }
        if let Some(usage) = payload.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choices) = payload.get("choices").and_then(Value::as_array) else {
            if payload.get("choices").is_some() {
                return Err(provider_chat_stream_malformed_error("choices_invalid"));
            }
            // 只带 usage、没有 choices 的块是 OpenAI include_usage 扩展允许的，直接跳过。
            return Ok(());
        };
        if choices.len() > 1 {
            return Err(provider_chat_stream_malformed_error("multiple_choices_unsupported"));
        }
        for choice in choices {
            if !choice.is_object() {
                return Err(provider_chat_stream_malformed_error("choice_invalid"));
            }
            let index = stream_index(choice.get("index"), "choice_index_invalid")?;
            if index != 0 {
                return Err(provider_chat_stream_malformed_error("multiple_choices_unsupported"));
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_string());
            }
            let delta = match choice.get("delta") {
                None | Some(Value::Null) => continue,
                Some(Value::Object(delta)) => delta,
                Some(_) => return Err(provider_chat_stream_malformed_error("delta_invalid")),
            };
            if delta
                .get("role")
                .is_some_and(|role| !role.is_null() && role.as_str() != Some("assistant"))
            {
                return Err(provider_chat_stream_malformed_error("role_invalid"));
            }
            if delta.get("content").is_some_and(|content| !content.is_null() && !content.is_string()) {
                return Err(provider_chat_stream_malformed_error("content_invalid"));
            }
            if delta.get("tool_calls").is_some_and(|calls| !calls.is_null() && !calls.is_array()) {
                return Err(provider_chat_stream_malformed_error("tool_calls_invalid"));
            }
            // 兼容端点可能在同一块里用多个键携带同一段 reasoning（实测双键同文），故只取第一个非空键。
            if let Some((field, reasoning)) = chat_reasoning_text(delta) {
                self.reasoning_field.get_or_insert_with(|| field.to_string());
                if !reasoning.is_empty() {
                    self.reasoning_content.push_str(reasoning);
                    (self.on_event)(ProviderStreamEvent::ReasoningTextDelta { delta: reasoning.to_string() });
                }
            }
            if let Some(details) = delta.get("reasoning_details").filter(|value| !value.is_null()) {
                let details = details
                    .as_array()
                    .ok_or_else(|| provider_chat_stream_malformed_error("reasoning_details_not_array"))?;
                self.receive_reasoning_details(details)?;
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                self.content.push_str(text);
                (self.on_event)(ProviderStreamEvent::OutputTextDelta { delta: text.to_string() });
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    self.receive_tool_call_fragment(call)?;
                }
            }
        }
        Ok(())
    }

    fn protocol_complete(&self) -> bool {
        self.done
    }
}

impl<'a> ChatSseDecoder<'a> {
    fn finish(self, result: Result<(), ProviderError>) -> ModelTurnResponse {
        let stop_reason = result.and_then(|()| {
            if !self.done {
                return Err(provider_chat_stream_malformed_error("terminal_done_missing"));
            }
            match self.finish_reason.as_deref() {
                Some("length") => Ok(ModelStopReason::Length),
                Some("stop" | "tool_calls" | "function_call") => Ok(ModelStopReason::Stop),
                Some("content_filter") => {
                    Err(provider_content_filter_error("provider Chat response was stopped by content filter"))
                }
                Some("network_error") => {
                    Err(provider_finish_network_error("provider Chat response reported a network error"))
                }
                Some(unknown) => Err(provider_chat_finish_reason_unsupported(unknown)),
                None => Err(provider_chat_stream_malformed_error("finish_reason_missing")),
            }
        });
        let thinking = if self.reasoning_content.is_empty() {
            self.reasoning_details
                .iter()
                .filter_map(|detail| detail.as_object().and_then(chat_reasoning_detail_text))
                .collect()
        } else {
            self.reasoning_content
        };
        let stop_reason = match stop_reason {
            Ok(reason) => reason,
            Err(error) => {
                return ModelTurnResponse::failed(
                    error,
                    Message::assistant(self.content, thinking, Vec::new(), None),
                );
            }
        };
        let tool_calls = self
            .tool_calls
            .into_values()
            .map(|call| ModelToolCall {
                tool_call_id: call.id,
                tool_name: call.name,
                arguments: parse_tool_arguments(&call.arguments),
                metadata: None,
            })
            .collect();
        let replay = (self.reasoning_field.is_some() || !self.reasoning_details.is_empty()).then(|| {
            ProviderMessageMetadata::Chat {
                provider_name: self.config.provider_name.clone(),
                model_name: self.model_name.to_string(),
                endpoint: self.endpoint.to_string(),
                reasoning_field: self
                    .reasoning_field
                    .unwrap_or_else(|| crate::types::DEFAULT_CHAT_REASONING_FIELD.into()),
                reasoning_details: self.reasoning_details,
            }
        });
        finalize_provider_response(ModelTurnResponse {
            assistant_message: Message::assistant(self.content, thinking, tool_calls, replay),
            usage: parse_usage(
                self.usage.as_ref(),
                "prompt_tokens",
                "completion_tokens",
                "/prompt_tokens_details/cached_tokens",
            ),
            stop_reason,
        })
    }

    fn new(
        on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send),
        config: &'a OpenAiProviderConfig,
        model_name: &'a str,
        endpoint: &'a str,
    ) -> Self {
        Self {
            config,
            model_name,
            endpoint,
            content: String::new(),
            reasoning_content: String::new(),
            reasoning_field: None,
            reasoning_details: Vec::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            done: false,
            on_event,
        }
    }

    /// 接收一组 reasoning_details 片段：检查格式后合并到已有片段。提供方没有通过
    /// reasoning_content / reasoning 给出思考文本时，才把片段里的文本作为思考增量发出；
    /// 其他字段（包括加密数据）只保存，不显示。
    fn receive_reasoning_details(&mut self, details: &[Value]) -> Result<(), ProviderError> {
        for detail in details {
            let Some(detail) = detail.as_object() else {
                return Err(provider_chat_stream_malformed_error("reasoning_detail_not_object"));
            };
            if self.reasoning_content.is_empty()
                && let Some(text) = chat_reasoning_detail_text(detail)
            {
                (self.on_event)(ProviderStreamEvent::ReasoningTextDelta { delta: text.to_string() });
            }
            append_reasoning_detail(&mut self.reasoning_details, detail);
        }
        Ok(())
    }

    /// 校验并拼接工具调用分片；名称或参数有增量时，通知调用方已收到输出。
    fn receive_tool_call_fragment(&mut self, call: &Value) -> Result<(), ProviderError> {
        // 空串等同于「本分片没有声明类型」：部分提供方只在第一个分片声明 function，
        // 后续参数分片重复该字段但留空。不是字符串的仍然拒绝。
        if !call.is_object()
            || call.get("type").is_some_and(|kind| !matches!(kind.as_str(), Some("") | Some("function")))
        {
            return Err(provider_chat_stream_malformed_error("tool_call_type_invalid"));
        }
        let function = call.get("function");
        if function.is_some_and(|function| !function.is_null() && !function.is_object()) {
            return Err(provider_chat_stream_malformed_error("tool_function_invalid"));
        }
        let name = function.and_then(|function| function.get("name"));
        let arguments = function.and_then(|function| function.get("arguments"));
        if [name, arguments]
            .into_iter()
            .flatten()
            .any(|value| !value.is_null() && !value.is_string())
        {
            return Err(provider_chat_stream_malformed_error("tool_function_field_invalid"));
        }
        let name = name.and_then(Value::as_str).unwrap_or_default();
        let arguments = arguments.and_then(Value::as_str).unwrap_or_default();
        // 工具分片要能唯一归到一个调用：省略 index 的单个调用按兼容保留，非法
        // 索引直接拒绝，不把不同调用拼进同一个聚合槽。
        let index = stream_index(call.get("index"), "tool_call_index_invalid")?;
        let entry = self.tool_calls.entry(index).or_default();
        if let Some(id) = call.get("id").and_then(Value::as_str)
            && entry.id.is_empty()
        {
            entry.id = id.to_string();
        }
        entry.name.push_str(name);
        entry.arguments.push_str(arguments);
        if !name.is_empty() || !arguments.is_empty() {
            (self.on_event)(ProviderStreamEvent::ToolCallDelta);
        }
        Ok(())
    }
}

/// 合并同一文本/摘要片段的增量；加密条目保持原有的边界和字段。
fn append_reasoning_detail(details: &mut Vec<Value>, incoming: &serde_json::Map<String, Value>) {
    let text_key = chat_reasoning_detail_text_field(incoming);
    if let (Some(key), Some(previous)) = (text_key, details.last_mut()) {
        let same_segment = previous.get("type") == incoming.get("type")
            && ["id", "index"].iter().all(|key| {
                match (
                    previous.get(*key).filter(|value| !value.is_null()),
                    incoming.get(*key).filter(|value| !value.is_null()),
                ) {
                    (Some(left), Some(right)) => left == right,
                    _ => true,
                }
            });
        if same_segment
            && let (Some(Value::String(existing)), Some(delta)) =
                (previous.get_mut(key), incoming.get(key).and_then(Value::as_str))
        {
            existing.push_str(delta);
            for (field, value) in incoming {
                if previous
                    .get(field)
                    .is_none_or(|existing| existing.is_null() || existing.as_str() == Some(""))
                {
                    previous[field] = value.clone();
                }
            }
            return;
        }
    }
    details.push(Value::Object(incoming.clone()));
}

fn provider_chat_stream_malformed_error(reason: &'static str) -> ProviderError {
    provider_stream_malformed_error("provider Chat stream was malformed", reason)
}

/// 读取 index 字段：省略时按 0 处理（兼容只有一个 choice 或工具调用的实现）；
/// 写了但不是非负整数（负数、字符串、null）时报格式错误，不当作 0。
fn stream_index(value: Option<&Value>, malformed: &'static str) -> Result<u64, ProviderError> {
    match value {
        None => Ok(0),
        Some(value) => value.as_u64().ok_or_else(|| provider_chat_stream_malformed_error(malformed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;

    #[test]
    fn interrupted_stream_keeps_text_without_executing_partial_tools() {
        let config = OpenAiProviderConfig {
            provider_name: "provider".into(),
            base_url: "https://example.com".into(),
            api_key: String::new(),
        };
        for kind in [ModelErrorKind::Cancelled, ModelErrorKind::JsonSchemaViolation] {
            let mut on_event = |_| {};
            let mut decoder =
                ChatSseDecoder::new(&mut on_event, &config, "model", "https://example.com/chat/completions");
            decoder.dispatch_event(br#"{"choices":[{"index":0,"delta":{"content":"partial","reasoning_content":"thinking","tool_calls":[{"index":0,"id":"call","function":{"name":"bash","arguments":"{"}}]}}]}"#).expect("valid stream fragment");
            let response = decoder.finish(Err(ProviderError::new(kind, "stream ended")));
            assert_eq!(response.assistant_message.content_text(), "partial");
            assert!(response.assistant_message.content().iter().any(
                |block| matches!(block, ContentBlock::Thinking { thinking, .. } if thinking == "thinking")
            ));
            assert_eq!(response.assistant_message.tool_calls().count(), 0);
            assert_eq!(response.error().map(|error| error.kind), Some(kind));
            assert_eq!(response.usage, None);
        }
    }
}

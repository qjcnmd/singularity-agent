//! 配置编辑器使用的只读模型元数据。已保存配置始终是权威来源。

use std::collections::BTreeMap;

use serde_json::Value;
use singularity_protocol::{DiscoveredModel, ReasoningVariant};

use super::{
    ProviderError, user_config_error, validate_base_url, validate_identifier, validate_model_id,
};
use crate::ModelErrorKind;

/// 查询模型目录并补齐元数据：URL 解释、请求构造、发送与结果补全都在这里。
/// 调用方只提供编辑器取值与已解析凭据，不转交 HTTP 半成品。
pub async fn discover(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let base_url = crate::openai::canonical_base_url(base_url);
    validate_base_url(base_url)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            // 客户端构造失败与请求失败是两种事实：保留底层来源以便定位。
            user_config_error(format!(
                "模型查询客户端无法启动（{}）。",
                discovery_error_source(error)
            ))
        })?;
    let request = client.get(crate::openai::models_endpoint(base_url));
    // 未保存的密钥是本次查询的纯输入；缺省时用已存储的密钥（可能为空）。
    let request = if api_key.is_empty() {
        request
    } else {
        request.bearer_auth(api_key)
    };
    let response = request.send().await.map_err(discovery_transport_error)?;
    if !response.status().is_success() {
        return Err(discovery_http_error(response.status().as_u16()));
    }
    let body: Value = read_response_body(response).await?;
    let mut models = read_listing(&body)?;
    if models.iter().any(|model| {
        model.max_context_tokens.is_none()
            || model.max_output_tokens.is_none()
            || model.reasoning_variants.is_empty()
    }) {
        // 这个公开目录请求既不携带用户 endpoint，也不携带凭据。
        if let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
            && let Ok(response) = client.get("https://models.dev/api.json").send().await
            && response.status().is_success()
            && let Ok(directory) = response.json::<Value>().await
        {
            supplement(&mut models, base_url, &directory);
        }
    }
    Ok(models)
}

/// 响应体读取与解码分两步：先取字节（传输事实），再自行解码（结构事实）。
/// reqwest 把 body 读取错误也包成 decode 类（0.12 中 body 断流的 is_decode()
/// 同样为 true），因此 Response::json 的单一 map_err 无法区分「body 传输超时/
/// 断流」与「提供方返回了无效 JSON」；这里用阶段本身区分，两类失败各自保持
/// 原有类别。
async fn read_response_body(response: reqwest::Response) -> Result<Value, ProviderError> {
    let bytes = response.bytes().await.map_err(discovery_transport_error)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| discovery_response_error("提供方未返回有效的模型目录。仍可手动添加模型。"))
}

/// 发现请求的传输失败（发送或 body 读取）：超时与断流各自保留原有类别，
/// 并附带去 URL、截断后的来源文本。本路径不重试。
fn discovery_transport_error(error: reqwest::Error) -> ProviderError {
    let kind = crate::error::provider_error_kind_for_transport(&error);
    ProviderError::new(
        kind,
        format!(
            "无法连接模型目录（{}），请检查网络或稍后重试；仍可手动添加模型。",
            discovery_error_source(error)
        ),
    )
    .with_code("model_discovery_transport_failed")
}

/// 有界、脱敏的来源文本：reqwest 的 Display 只给出大类（body 读取失败统一是
/// "error decoding response body"），具体原因在来源链末端（超时、断流等），
/// 因此取最内层描述；去掉 URL（凭据只在请求头，本就不会进入错误文本）后按
/// 共用上限折叠空白并截断。
fn discovery_error_source(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut source: &(dyn std::error::Error + 'static) = &error;
    while let Some(inner) = source.source() {
        source = inner;
    }
    crate::error::bounded_provider_error_diagnostic(&source.to_string())
}

fn read_listing(body: &Value) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let entries: Vec<(&str, &Value)> =
        if let Some(data) = body.get("data").and_then(Value::as_array) {
            data.iter()
                .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
                .collect()
        } else if let Some(models) = body.get("models").and_then(Value::as_object) {
            models
                .iter()
                .map(|(id, entry)| (id.as_str(), entry))
                .collect()
        } else {
            return Err(discovery_response_error(
                "提供方未返回 data 模型列表或 models 目录。仍可手动添加模型。",
            ));
        };
    let mut models = BTreeMap::new();
    for (id, entry) in entries {
        if entry.is_object() && validate_model_id(id, "model id").is_ok() {
            models
                .entry(id.to_string())
                .or_insert_with(|| metadata(id, entry));
        }
    }
    Ok(models.into_values().collect())
}

fn discovery_response_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ModelErrorKind::JsonSchemaViolation, message)
        .with_code("model_discovery_response_invalid")
}

fn discovery_http_error(status: u16) -> ProviderError {
    // 发现请求不重试，因此直接使用共同状态分类：409 在这里就是输入错误，
    // 600 以上是未知；生成路径的可重试 409 与过载 600+ 是它自己的显式例外。
    ProviderError::new(
        crate::error::provider_error_kind_for_http_status(status),
        format!("获取模型失败：HTTP {status}。仍可手动添加模型。"),
    )
    .with_code("model_discovery_http_status")
}

fn metadata(id: &str, entry: &Value) -> DiscoveredModel {
    let label = |paths: &[&str]| {
        paths.iter().find_map(|path| {
            entry
                .pointer(path)?
                .as_str()
                .filter(|text| !text.is_empty())
        })
    };
    let capacity = |paths: &[&str]| {
        paths.iter().find_map(|path| {
            u32::try_from(entry.pointer(path)?.as_u64()?)
                .ok()
                .filter(|value| *value > 0)
        })
    };
    let mut efforts = Vec::new();
    if let Some(values) = entry.get("reasoning_efforts").and_then(Value::as_array) {
        efforts.extend(values.iter().filter_map(Value::as_str));
    }
    if let Some(options) = entry.get("reasoning_options").and_then(Value::as_array) {
        for option in options {
            if option.get("type").and_then(Value::as_str) == Some("effort")
                && let Some(values) = option.get("values").and_then(Value::as_array)
            {
                efforts.extend(values.iter().filter_map(Value::as_str));
            }
        }
    }
    let mut reasoning_variants = Vec::new();
    for effort in efforts {
        if !matches!(effort, "default" | "off")
            && validate_identifier(effort, "reasoning effort").is_ok()
            && !reasoning_variants
                .iter()
                .any(|variant: &ReasoningVariant| variant.id == effort)
        {
            reasoning_variants.push(ReasoningVariant {
                id: effort.to_string(),
                enabled: true,
                wire_effort: Some(effort.to_string()),
            });
        }
    }
    // 优先采用对方声明的 default，其次 medium；这也是导入配置的默认值。
    let default_variant = label(&["/default_reasoning_effort"])
        .filter(|id| reasoning_variants.iter().any(|variant| variant.id == *id))
        .or_else(|| {
            reasoning_variants
                .iter()
                .find(|variant| variant.id == "medium")
                .map(|variant| variant.id.as_str())
        })
        .or_else(|| {
            reasoning_variants
                .first()
                .map(|variant| variant.id.as_str())
        })
        .map(str::to_string);
    DiscoveredModel {
        model_id: id.to_string(),
        display_name: label(&["/name", "/display_name", "/displayName"]).map(str::to_string),
        max_context_tokens: capacity(&[
            "/context_window",
            "/context_length",
            "/contextWindow",
            "/max_input_tokens",
            "/limit/context",
        ]),
        max_output_tokens: capacity(&[
            "/max_output_tokens",
            "/maxOutputTokens",
            "/max_tokens",
            "/maxTokens",
            "/limit/output",
            "/top_provider/max_completion_tokens",
        ]),
        reasoning_variants,
        default_variant,
    }
}

/// 目录条目按 `api` 地址对齐本机配置的地址：地址是用户填的，程序不认识任何
/// 厂商，也不为缺 `api` 字段的条目补写死的地址。
fn supplement(models: &mut [DiscoveredModel], base_url: &str, directory: &Value) {
    let Some(providers) = directory.as_object() else {
        return;
    };
    // 与推理共用同一地址解释：目录补齐的根才是 models.dev 记录的 api 值。
    let endpoint = crate::openai::api_root(base_url);
    let Some((_, provider)) = providers.iter().find(|(_, provider)| {
        provider
            .get("api")
            .and_then(Value::as_str)
            .is_some_and(|api| crate::openai::canonical_base_url(api) == endpoint)
    }) else {
        return;
    };
    for model in models {
        let Some(entry) = provider
            .get("models")
            .and_then(|models| models.get(&model.model_id))
        else {
            continue;
        };
        let known = metadata(&model.model_id, entry);
        model.display_name = model.display_name.take().or(known.display_name);
        model.max_context_tokens = model.max_context_tokens.or(known.max_context_tokens);
        model.max_output_tokens = model.max_output_tokens.or(known.max_output_tokens);
        // 思考词形是各家的 wire 差异，目录不提供；这里只补档位，词形留给用户声明。
        if model.reasoning_variants.is_empty() && !known.reasoning_variants.is_empty() {
            model.reasoning_variants = known.reasoning_variants;
            model.default_variant = known.default_variant;
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixture assertions follow the owning module's convention.
mod tests {
    use super::*;
    use crate::http_test_support::spawn_http_server;
    use serde_json::json;

    #[test]
    fn listing_preserves_aliases_and_only_advertised_efforts() {
        let models = read_listing(&json!({"models": {
            "gateway-alias": {"id": "canonical-id", "limit": {"context": 128000, "output": 8192}, "reasoning_options": [{"type": "effort", "values": ["low", "xhigh", "low", null, "default"]}]},
            "unknown": {"reasoning": true}, "version": 1
        }})).expect("valid model listing");
        assert_eq!(models[0].model_id, "gateway-alias");
        assert_eq!(models[0].max_context_tokens, Some(128000));
        assert_eq!(
            models[0]
                .reasoning_variants
                .iter()
                .map(|variant| variant.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "xhigh"]
        );
        assert!(models[1].reasoning_variants.is_empty());
    }

    #[test]
    fn supplement_requires_exact_route_and_preserves_endpoint_facts() {
        let directory = json!({"alibaba-cn": {"api": "https://dashscope.aliyuncs.com/compatible-mode/v1", "models": {
            "example": {"limit": {"context": 1000000}, "reasoning_options": [{"type": "effort", "values": ["low", "medium", "xhigh"]}]}
        }}});
        let mut models = read_listing(
            &json!({"data": [{"id": "example", "context_length": 64000}, {"id": "example-alias"}]}),
        )
        .expect("valid model listing");
        supplement(&mut models, "https://proxy.invalid/v1", &directory);
        assert!(models[0].reasoning_variants.is_empty());
        supplement(
            &mut models,
            "https://dashscope.aliyuncs.com/compatible-mode/v1/",
            &directory,
        );
        assert_eq!(models[0].max_context_tokens, Some(64000));
        assert_eq!(models[0].default_variant.as_deref(), Some("medium"));
        assert!(models[1].reasoning_variants.is_empty());

        // 写明端点的同一地址解释出同一个根：目录补齐不再依赖编辑器先清理输入。
        let mut by_endpoint =
            read_listing(&json!({"data": [{"id": "example", "context_length": 64000}]}))
                .expect("valid model listing");
        supplement(
            &mut by_endpoint,
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
            &directory,
        );
        assert_eq!(by_endpoint[0].default_variant.as_deref(), Some("medium"));
    }

    #[test]
    fn http_status_preserves_actionable_failure_category() {
        // 类别映射由共享 HTTP 分类用例覆盖；这里只钉住发现路径特有的稳定码与可操作信息。
        for status in [401, 404, 503] {
            let error = discovery_http_error(status);
            assert_eq!(error.code.as_deref(), Some("model_discovery_http_status"));
            assert!(error.message.contains(&status.to_string()));
        }
    }

    #[test]
    fn malformed_listing_is_a_response_schema_failure() {
        let error = read_listing(&json!({"unexpected": []})).expect_err("invalid listing");
        assert_eq!(error.kind, ModelErrorKind::JsonSchemaViolation);
        assert_eq!(
            error.code.as_deref(),
            Some("model_discovery_response_invalid")
        );
    }

    /// 发现查询自己解释地址、自己携带凭据：写明的端点被剥到根，请求真实发出。
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn discovery_requests_the_root_of_the_given_base_url() {
        use std::io::Write;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        for (suffix, expected_path) in [("", "/models"), ("/v1/chat/completions", "/v1/models")] {
            let (address, server) = spawn_http_server(move |mut stream, request| {
                // 元数据完整（上下文、输出与推理档位都有），不会触发目录补齐。
                let body = json!({"data": [{
                    "id": "example",
                    "context_window": 128000,
                    "max_output_tokens": 4096,
                    "reasoning_efforts": ["low"]
                }]})
                .to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                let authorization = request.header("authorization").map(str::to_string);
                (request.target, authorization)
            });

            let base_url = format!("http://{address}{suffix}");
            let models = runtime
                .block_on(discover(&base_url, "explicit-key"))
                .expect("discovery succeeds");
            let (path, authorization) = server.join().unwrap();
            assert_eq!(path, expected_path, "base url: {base_url}");
            assert_eq!(authorization.as_deref(), Some("Bearer explicit-key"));
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].model_id, "example");
            assert_eq!(models[0].max_context_tokens, Some(128000));
        }
    }

    /// 未知地址形状在任何网络动作前被拒绝。
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn discovery_rejects_an_unsupported_base_url_before_sending() {
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(discover("ftp://example.invalid/v1", ""))
            .expect_err("unsupported scheme");
        assert_eq!(error.kind, ModelErrorKind::InvalidRequest);
    }

    /// body 传输超时仍是超时：响应头已到、body 停在中途时不能改判成提供方
    /// 返回了无效 JSON。
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn body_timeout_keeps_the_transport_category() {
        let (address, server) = spawn_scripted_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n",
            b"{\"data\":[",
            ServerTail::Hold,
        );
        let error = read_body_with_short_timeout(address)
            .await
            .expect_err("body timeout");
        server.join().unwrap();
        assert_eq!(error.kind, ModelErrorKind::Timeout, "{error:?}");
        assert_eq!(
            error.code.as_deref(),
            Some("model_discovery_transport_failed")
        );
    }

    /// body 断流（Content-Length 未满足）同样是网络故障，不是 JSON 结构错误。
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn truncated_body_keeps_the_transport_category() {
        let (address, server) = spawn_scripted_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n",
            b"{\"data\":[",
            ServerTail::Close,
        );
        let error = read_body_with_short_timeout(address)
            .await
            .expect_err("truncated body");
        server.join().unwrap();
        assert_eq!(error.kind, ModelErrorKind::NetworkError, "{error:?}");
        assert_eq!(
            error.code.as_deref(),
            Some("model_discovery_transport_failed")
        );
    }

    /// 完整读取后无法解码才是响应结构失败。
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn complete_but_malformed_body_is_a_response_schema_failure() {
        let (address, server) = spawn_scripted_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8\r\n\r\n",
            b"not json",
            ServerTail::Close,
        );
        let error = read_body_with_short_timeout(address)
            .await
            .expect_err("malformed body");
        server.join().unwrap();
        assert_eq!(error.kind, ModelErrorKind::JsonSchemaViolation, "{error:?}");
        assert_eq!(
            error.code.as_deref(),
            Some("model_discovery_response_invalid")
        );
    }

    /// 脚本化的响应尾部行为。
    enum ServerTail {
        /// 保持连接直到客户端放弃：让客户端总超时在读取 body 时触发。
        Hold,
        /// 立刻关闭连接：制造 Content-Length 未满足的断流。
        Close,
    }

    /// 单次原始 HTTP 服务器：读到请求头后按脚本写出响应头与部分 body。
    fn spawn_scripted_server(
        head: &'static str,
        body: &'static [u8],
        tail: ServerTail,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};

        spawn_http_server(move |mut stream, _| {
            stream.write_all(head.as_bytes()).expect("write head");
            stream.write_all(body).expect("write body");
            if matches!(tail, ServerTail::Hold) {
                let mut sink = [0u8; 256];
                while let Ok(read) = stream.read(&mut sink) {
                    if read == 0 {
                        break;
                    }
                }
            }
        })
    }

    /// 用短超时客户端取回响应头，再把响应交给被测的 body 读取步骤：发现路径
    /// 自身的 20 秒总超时无法在测试里等待，而超时事实属于客户端配置。
    async fn read_body_with_short_timeout(
        address: std::net::SocketAddr,
    ) -> Result<Value, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .expect("test client");
        let response = client
            .get(format!("http://{address}/models"))
            .send()
            .await
            .expect("response head");
        read_response_body(response).await
    }
}

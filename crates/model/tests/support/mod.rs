#![allow(dead_code, unused_imports)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

pub(crate) use serde_json::{Value, json};
pub(crate) use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelBlockerKind, ModelError,
    ModelErrorCategory, ModelErrorKind, ModelMessage, ModelProviderConfig, ModelRole,
    ModelToolCall, ModelToolParseStatus, ModelToolSchema, ModelTurnRequest, ModelTurnResponse,
    ModelTurnStatus, ModelUsage, OpenAiProvider, OpenAiProviderConfig, Provider,
    ProviderApiProtocol, ProviderAttemptEvent, ProviderConfigSnapshot, ProviderConfigSource,
    ProviderConfigurationStatus, ProviderError, ProviderErrorStage, ProviderProtocolContract,
    ProviderReasoningReplay, ProviderStreamEvent, ProviderStreamingCapability, ToolChoicePolicy,
    chat_completions_endpoint, responses_endpoint, validate_model_request,
    validate_model_request_with_capabilities, validate_model_response,
    validate_model_turn_response, validate_provider_config,
};
pub(crate) use std::io::{BufRead, BufReader, Read, Write};
pub(crate) use std::net::{TcpListener, TcpStream};
pub(crate) use std::sync::{
    Mutex,
    mpsc::{self, Receiver},
};
pub(crate) use std::thread;
pub(crate) use std::time::Duration;
pub(crate) use tempfile::tempdir;

pub(crate) fn tool_call(id: &str, name: &str) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        arguments: serde_json::json!({"path": "README.md"}),
        raw_arguments: r#"{"path":"README.md"}"#.to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }
}

/// 进程环境写锁：所有测试对 SINGULARITY_HOME 等环境变量的修改必须经同一
/// 把锁串行化（Rust 测试默认并行运行，直接 set_var 会互相覆盖）。
pub(crate) static PROCESS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 安装环境变量的 RAII guard：作用域结束时移除该变量并释放环境写锁。
pub(crate) struct ProcessEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    key: String,
}

impl Drop for ProcessEnvGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(&self.key) };
    }
}

/// 在环境写锁内设置 `key` 为 `value`；返回的 guard 在作用域结束时移除。
/// 测试失败会留下 poisoned 锁，这里按中毒状态继续持有（环境串行化是
/// 尽力而为，测试失败本身已是要报告的结果）。
pub(crate) fn install_process_env(key: &str, value: &std::path::Path) -> ProcessEnvGuard {
    let _lock = PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    unsafe { std::env::set_var(key, value) };
    ProcessEnvGuard {
        _lock,
        key: key.to_string(),
    }
}

/// config.json 用户配置 fixture：以 `SINGULARITY_HOME` 指向隔离目录，
/// 一次性写入 config.json 与 auth.json（ProviderConfigSnapshot::capture
/// 读取这两个文件；读取层直接读进程环境里的 SINGULARITY_HOME，因此必须经
/// `install_env` 把它安装到进程环境）。
pub(crate) struct UserConfigFixture {
    pub(crate) directory: tempfile::TempDir,
}

/// 安装 SINGULARITY_HOME 的 RAII guard：作用域结束时移除，避免污染并行测试。
pub(crate) struct UserConfigEnvGuard {
    _process_env: ProcessEnvGuard,
}

impl UserConfigFixture {
    pub(crate) fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("user config directory"),
        }
    }

    /// 把 SINGULARITY_HOME 安装到进程环境；返回的 guard 在作用域结束时移除。
    pub(crate) fn install_env(&self) -> UserConfigEnvGuard {
        let _process_env = install_process_env("SINGULARITY_HOME", self.directory.path());
        UserConfigEnvGuard { _process_env }
    }

    /// 写入 config.json：`default_model` 为 `provider/model[#variant]` 选择器，
    /// `providers` 为 config.json providers 段（`name -> {base_url, models}`）。
    pub(crate) fn write_config(&self, default_model: &str, providers: Value) {
        let default_provider = default_model.split('/').next().expect("provider id");
        let config = json!({
            "version": 1,
            "default_provider": default_provider,
            "default_model": default_model,
            "providers": providers,
        });
        std::fs::write(
            self.directory.path().join("config.json"),
            config.to_string(),
        )
        .expect("write user config");
    }

    /// 写入 auth.json：给指定 provider 设置测试 api key；多次调用按
    /// provider 合并（保留既有条目）。
    pub(crate) fn set_api_key(&self, provider: &str, api_key: &str) {
        let path = self.directory.path().join("auth.json");
        let mut auth: Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| json!({ "schema_version": 1, "providers": {} }));
        auth["providers"][provider] = json!({ "api_key": api_key });
        std::fs::write(&path, auth.to_string()).expect("write user auth");
        // 读取层按文件 mode 做 owner-only fail-closed 校验，fixture 须以 0600 落盘。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict user auth to owner");
        }
    }
}

pub(crate) fn provider_test_config(base_url: String) -> OpenAiProviderConfig {
    provider_config_with_base_url(chat_completions_endpoint(&base_url))
}

/// 测试共享的注入 runtime：provider 的异步执行一律由上层提供。
pub(crate) fn test_runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("shared test provider runtime")
        })
        .handle()
        .clone()
}

/// 构造注入共享测试 runtime 的 provider；返回 Result 以保持调用点 `.expect` 形状。
/// 协议按 base_url 端点后缀选择（`/responses` 结尾为 Responses，否则 Chat），
/// 与目录无 reasoning 变体条目的选择同形。
pub(crate) fn test_provider(config: OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError> {
    test_provider_with_runtime(config, test_runtime_handle())
}

/// [`test_provider`] 的显式 runtime 变体：验证 provider 使用调用方注入的 runtime。
pub(crate) fn test_provider_with_runtime(
    config: OpenAiProviderConfig,
    runtime_handle: tokio::runtime::Handle,
) -> Result<OpenAiProvider, ProviderError> {
    let api_protocol = if config
        .base_url
        .trim()
        .trim_end_matches('/')
        .ends_with("/responses")
    {
        ProviderApiProtocol::OpenAiResponses
    } else {
        ProviderApiProtocol::OpenAiChatCompletions
    };
    OpenAiProvider::with_single_model(config, api_protocol, runtime_handle)
}

pub(crate) fn provider_auto_test_config(base_url: String) -> OpenAiProviderConfig {
    provider_config_with_base_url(base_url)
}

pub(crate) fn provider_config_with_base_url(base_url: String) -> OpenAiProviderConfig {
    OpenAiProviderConfig {
        provider_name: "openai_compatible".to_string(),
        model_name: "test-model".to_string(),
        base_url,
        api_key: "test-key-placeholder".to_string(),
        source: ProviderConfigSource::UserConfigFile,
        max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    }
}

pub(crate) fn capability_test_request(
    model_name: Option<&str>,
    strict: bool,
    max_tool_calls: u32,
) -> ModelTurnRequest {
    let mut request = ModelTurnRequest::new(
        "request_capability_test",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    if let Some(model_name) = model_name {
        request.model_preferences.model_name = Some(model_name.to_string());
    }
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    });
    request.tool_choice.max_tool_calls = max_tool_calls;
    request.tool_choice.strict_tool_schema = strict;
    request
}

pub(crate) fn single_response_server(status_line: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test provider");
    let addr = listener.local_addr().expect("test provider address");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept test provider request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let (first_line, headers, _) = read_provider_request(&mut reader);
        assert!(first_line.contains("/v1/chat/completions"));
        assert!(headers.contains("authorization: Bearer test-key-placeholder"));
        write_provider_response(&mut stream, status_line, body, false);
    });
    format!("http://{addr}")
}

pub(crate) fn captured_request_server(
    status_line: &'static str,
    body: &'static str,
) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test provider");
    let addr = listener.local_addr().expect("test provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept test provider request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let (first_line, headers, request_body) = read_provider_request(&mut reader);
        assert!(first_line.contains("/v1/chat/completions"));
        assert!(headers.contains("authorization: Bearer test-key-placeholder"));
        tx.send(request_body).expect("send request body");
        write_provider_response(&mut stream, status_line, body, true);
    });
    (format!("http://{addr}"), rx)
}

pub(crate) fn responses_provider_server(
    actual_body: serde_json::Value,
) -> (String, Receiver<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Responses provider");
    let addr = listener.local_addr().expect("Responses provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Responses request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let (first_line, headers, request_body) = read_provider_request(&mut reader);
        assert!(first_line.contains("/v1/responses"));
        assert!(headers.contains("authorization: Bearer test-key-placeholder"));
        let body = actual_body.to_string();
        write_provider_response(&mut stream, "HTTP/1.1 200 OK", &body, false);
        tx.send(vec![(first_line, request_body)])
            .expect("send Responses requests");
    });
    // Responses 端点显式 /v1/responses；目录条目按 api_protocol: responses 声明协议。
    (format!("http://{addr}/v1/responses"), rx)
}

pub(crate) fn chat_stream_server(
    chunks: Vec<Vec<u8>>,
    declared_length: Option<usize>,
) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Chat stream provider");
    let addr = listener.local_addr().expect("Chat stream provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Chat stream request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone Chat stream request"));
        let (first_line, headers, request_body) = read_provider_request(&mut reader);
        assert!(first_line.contains("/v1/chat/completions"));
        assert!(headers.contains("authorization: Bearer test-key-placeholder"));
        tx.send(request_body).expect("send Chat stream request");
        let length = declared_length
            .map(|length| format!("content-length: {length}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n{length}\r\n"
        )
        .expect("write Chat stream headers");
        stream.flush().expect("flush Chat stream headers");
        for chunk in chunks {
            stream.write_all(&chunk).expect("write Chat stream chunk");
            stream.flush().expect("flush Chat stream chunk");
            thread::sleep(Duration::from_millis(1));
        }
    });
    (format!("http://{addr}/v1/chat/completions"), rx)
}

pub(crate) fn responses_stream_server(
    chunks: Vec<Vec<u8>>,
    declared_length: Option<usize>,
) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Responses stream provider");
    let addr = listener
        .local_addr()
        .expect("Responses stream provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Responses stream request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let (first_line, headers, request_body) = read_provider_request(&mut reader);
        assert!(first_line.contains("/v1/responses"));
        assert!(headers.contains("authorization: Bearer test-key-placeholder"));
        tx.send(request_body)
            .expect("send Responses stream request");
        let length = declared_length
            .map(|length| format!("content-length: {length}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n{length}\r\n"
        )
        .expect("write Responses stream headers");
        stream.flush().expect("flush Responses stream headers");
        for chunk in chunks {
            stream
                .write_all(&chunk)
                .expect("write Responses stream chunk");
            stream.flush().expect("flush Responses stream chunk");
            thread::sleep(Duration::from_millis(1));
        }
    });
    (format!("http://{addr}/v1/responses"), rx)
}

/// Chat 实际响应：带 tool call 但没有 reasoning_content——在 ReplayReasoningContent
/// 模式下这是真实的回放义务违规（有 tool call 必须回放 reasoning_content）。
pub(crate) fn sequence_response_server(
    responses: Vec<(&'static str, &'static str)>,
) -> (String, Receiver<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sequence provider");
    let addr = listener.local_addr().expect("sequence provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for (attempt, (status_line, body)) in responses.into_iter().enumerate() {
            let (mut stream, _) = listener.accept().expect("accept sequence provider request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let (_, _, _) = read_provider_request(&mut reader);
            tx.send(attempt + 1).expect("send provider attempt");
            write_provider_response(&mut stream, status_line, body, true);
        }
    });
    (format!("http://{addr}"), rx)
}

pub(crate) fn sequence_response_server_with_headers(
    responses: Vec<(&'static str, &'static str, &'static str)>,
) -> (String, Receiver<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind header sequence provider");
    let addr = listener
        .local_addr()
        .expect("header sequence provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for (attempt, (status_line, body, headers)) in responses.into_iter().enumerate() {
            let (mut stream, _) = listener
                .accept()
                .expect("accept header sequence provider request");
            let mut reader =
                BufReader::new(stream.try_clone().expect("clone header sequence stream"));
            let (_, _, _) = read_provider_request(&mut reader);
            tx.send(attempt + 1).expect("send header provider attempt");
            write!(
                stream,
                "{status_line}\r\n{headers}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write header sequence provider response");
        }
    });
    (format!("http://{addr}"), rx)
}

pub(crate) fn read_provider_request(reader: &mut BufReader<TcpStream>) -> (String, String, String) {
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .expect("read request line");
    let mut headers = String::new();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read request header");
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().unwrap_or(0);
        }
        headers.push_str(&line);
    }
    let mut request_body = vec![0; content_length];
    reader
        .read_exact(&mut request_body)
        .expect("read request body");
    (
        first_line,
        headers,
        String::from_utf8(request_body).expect("utf8 request body"),
    )
}

pub(crate) fn write_provider_response(
    stream: &mut TcpStream,
    status_line: &str,
    body: &str,
    close: bool,
) {
    let connection = if close { "connection: close\r\n" } else { "" };
    write!(
        stream,
        "{status_line}\r\n{connection}content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("write provider response");
}

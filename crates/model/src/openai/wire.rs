use crate::{CHAT_COMPLETIONS_PATH, MODELS_PATH, RESPONSES_PATH};

/// 用户可以把 `base_url` 写成 API 根、版本根，或直接粘贴某个具体端点。
/// 对这些输入的解释只在本文件：`api_root` 剥掉已写明的端点得到根，
/// `endpoint_url` 按同一形状补齐目标路径。配置保存、模型发现与两种协议的
/// 推理都经过这里，不再各自猜测地址的含义。
const KNOWN_ENDPOINTS: [&str; 3] = [CHAT_COMPLETIONS_PATH, RESPONSES_PATH, MODELS_PATH];

/// 输入的规范形状：去首尾空白与结尾斜杠；不改变地址语义。
pub(crate) fn canonical_base_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

/// `base_url` 指向的 API 根：剥掉已写明的协议端点或目录端点。
pub(crate) fn api_root(base_url: &str) -> &str {
    let base = canonical_base_url(base_url);
    KNOWN_ENDPOINTS
        .iter()
        .find_map(|endpoint| base.strip_suffix(endpoint))
        .filter(|root| !root.is_empty())
        .unwrap_or(base)
}

/// 推理端点：已写明端点或以 `/v1` 结尾的根直接拼协议路径，其余补一个 `/v1`。
fn endpoint_url(base_url: &str, path: &str) -> String {
    let base = canonical_base_url(base_url);
    let root = api_root(base);
    // `api_root` 剥掉了写明的端点：长度变了就说明地址已写死，不再补版本段。
    if root != base || root.ends_with("/v1") {
        format!("{root}{path}")
    } else {
        format!("{root}/v1{path}")
    }
}

/// 将基础 URL 解析为兼容 OpenAI 的 Chat Completions 端点。
pub(crate) fn chat_completions_endpoint(base_url: &str) -> String {
    endpoint_url(base_url, CHAT_COMPLETIONS_PATH)
}

/// 将基础 URL 解析为兼容 OpenAI 的 Responses 端点。
pub(crate) fn responses_endpoint(base_url: &str) -> String {
    endpoint_url(base_url, RESPONSES_PATH)
}

/// 模型目录端点：与推理共用 `base_url` 的解释。目录始终随用户给出的根，
/// 不为它补 `/v1`——自定义前缀（如 `/api/paas/v4`）的提供方在根上提供列表。
pub(crate) fn models_endpoint(base_url: &str) -> String {
    format!("{}{MODELS_PATH}", api_root(base_url))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

    use super::{chat_completions_endpoint, models_endpoint, responses_endpoint};

    /// 同一 `base_url` 输入在三个消费点上的解释表：已能工作的路由逐项固定，
    /// 自定义前缀（`/api/paas/v4`）继续要求写明完整端点，不为其补 `/v1`。
    #[test]
    fn base_url_shapes_build_one_url_per_consumer() {
        for (input, chat, responses, models) in [
            // 版本根：推理补协议路径，目录直接挂在根上。
            (
                "https://x.invalid/v1",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/models",
            ),
            // 裸主机根：推理补 /v1（既有规则），目录随根。
            (
                "https://x.invalid",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/models",
            ),
            // 自定义前缀：不认作版本根，只有写明端点才可用。
            (
                "https://x.invalid/api/paas/v4",
                "https://x.invalid/api/paas/v4/v1/chat/completions",
                "https://x.invalid/api/paas/v4/v1/responses",
                "https://x.invalid/api/paas/v4/models",
            ),
            // 写明 Chat 端点：自身逐字使用，另一种协议换路径不换前缀。
            (
                "https://x.invalid/api/paas/v4/chat/completions",
                "https://x.invalid/api/paas/v4/chat/completions",
                "https://x.invalid/api/paas/v4/responses",
                "https://x.invalid/api/paas/v4/models",
            ),
            (
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/models",
            ),
            (
                "https://x.invalid/v1/models",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/models",
            ),
            // 形状差异不影响解释：结尾斜杠与首尾空白同样归一。
            (
                "  https://x.invalid/v1/  ",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/models",
            ),
        ] {
            assert_eq!(chat_completions_endpoint(input), chat, "chat: {input}");
            assert_eq!(responses_endpoint(input), responses, "responses: {input}");
            assert_eq!(models_endpoint(input), models, "models: {input}");
        }
    }
}

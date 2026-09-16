use crate::{CHAT_COMPLETIONS_PATH, MODELS_PATH, RESPONSES_PATH};

/// 已知的 OpenAI 协议端点；`base_url` 写明了其中一个时先剥掉它。
const KNOWN_ENDPOINTS: [&str; 3] = [CHAT_COMPLETIONS_PATH, RESPONSES_PATH, MODELS_PATH];

/// 输入的规范形状：去首尾空白与结尾斜杠；不改变地址语义。
pub(crate) fn canonical_base_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

/// `base_url` 指向的 API 根：三种端点都由这一个根拼出。
///
/// 规则只有一条：已知端点先被剥掉，剩下的路径就是根，逐字使用——裸主机
/// （`https://api.deepseek.com`）的根就是它本身，版本根与自定义前缀都按用户
/// 写的那样使用。中间层不替任何消费者暗补版本段，否则同一个 `base_url`
/// 在推理与目录之间会有两个含义。
///
/// 结果始终是 `base_url` 的切片，因此返回借用；只有真正需要拥有端点 URL 的
/// 调用方才分配。
pub(crate) fn api_root(base_url: &str) -> &str {
    let base = canonical_base_url(base_url);
    KNOWN_ENDPOINTS
        .iter()
        .find_map(|endpoint| base.strip_suffix(endpoint))
        .filter(|root| !root.is_empty())
        .unwrap_or(base)
}

/// 将基础 URL 解析为兼容 OpenAI 的 Chat Completions 端点。
pub(crate) fn chat_completions_endpoint(base_url: &str) -> String {
    format!("{}{CHAT_COMPLETIONS_PATH}", api_root(base_url))
}

/// 将基础 URL 解析为兼容 OpenAI 的 Responses 端点。
pub(crate) fn responses_endpoint(base_url: &str) -> String {
    format!("{}{RESPONSES_PATH}", api_root(base_url))
}

/// 模型目录端点：与推理共用同一个根。
pub(crate) fn models_endpoint(base_url: &str) -> String {
    format!("{}{MODELS_PATH}", api_root(base_url))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

    use super::{api_root, chat_completions_endpoint, models_endpoint, responses_endpoint};

    /// 同一 `base_url` 输入在三个消费点上解释成同一个根。
    ///
    /// 表中每个输入固定两件事：剥掉已知端点后剩下的根，以及根上拼出的三个
    /// 端点。中间层不暗补版本段，因此裸主机与自定义前缀都由用户写全。
    #[test]
    fn one_root_serves_inference_and_model_discovery() {
        for (input, root, chat, responses, models) in [
            // 版本根：三种端点都挂在它下面。
            (
                "https://x.invalid/v1",
                "https://x.invalid/v1",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/models",
            ),
            // 裸主机：根就是主机本身，端点直接挂上去，不替用户补 /v1。
            (
                "https://x.invalid",
                "https://x.invalid",
                "https://x.invalid/chat/completions",
                "https://x.invalid/responses",
                "https://x.invalid/models",
            ),
            // 自定义前缀：逐字作为根。
            (
                "https://x.invalid/api/paas/v4",
                "https://x.invalid/api/paas/v4",
                "https://x.invalid/api/paas/v4/chat/completions",
                "https://x.invalid/api/paas/v4/responses",
                "https://x.invalid/api/paas/v4/models",
            ),
            // 写明某个已知端点：剥掉它得到根，另一种协议换路径不换前缀。
            (
                "https://x.invalid/api/paas/v4/chat/completions",
                "https://x.invalid/api/paas/v4",
                "https://x.invalid/api/paas/v4/chat/completions",
                "https://x.invalid/api/paas/v4/responses",
                "https://x.invalid/api/paas/v4/models",
            ),
            (
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/models",
            ),
            (
                "https://x.invalid/v1/models",
                "https://x.invalid/v1",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/models",
            ),
            // 形状差异不影响解释：结尾斜杠与首尾空白同样归一。
            (
                "  https://x.invalid/v1/  ",
                "https://x.invalid/v1",
                "https://x.invalid/v1/chat/completions",
                "https://x.invalid/v1/responses",
                "https://x.invalid/v1/models",
            ),
        ] {
            assert_eq!(api_root(input), root, "root: {input}");
            assert_eq!(chat_completions_endpoint(input), chat, "chat: {input}");
            assert_eq!(responses_endpoint(input), responses, "responses: {input}");
            assert_eq!(models_endpoint(input), models, "models: {input}");
        }
    }

    /// 粘贴的端点在三个消费点上都逐字保留。
    #[test]
    fn a_pasted_endpoint_stays_byte_identical() {
        assert_eq!(
            chat_completions_endpoint("https://x.invalid/v1/chat/completions"),
            "https://x.invalid/v1/chat/completions"
        );
        assert_eq!(
            responses_endpoint("https://x.invalid/v1/responses"),
            "https://x.invalid/v1/responses"
        );
        assert_eq!(
            models_endpoint("https://x.invalid/v1/models"),
            "https://x.invalid/v1/models"
        );
    }
}

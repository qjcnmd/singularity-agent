//! JSON-RPC 方法表：方法名枚举、registry 与调用类型。

/// JSON-RPC method 的调用类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Request,
    Notification,
}

/// 单个 method 的名称、调用类型与结果形状。
#[derive(Clone, Copy)]
pub struct MethodSpec {
    pub method: Method,
    pub name: &'static str,
    pub kind: MethodKind,
}

impl std::fmt::Debug for MethodSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MethodSpec")
            .field("method", &self.method)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

macro_rules! method_registry {
    ($( $variant:ident => ($name:literal, $kind:ident) ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        /// JSON-RPC 方法名。
        pub enum Method {
            $( $variant, )+
        }

        /// 唯一的公共 method registry；方法查找由此生成。
        pub const METHOD_REGISTRY: &[MethodSpec] = &[
            $( MethodSpec {
                method: Method::$variant,
                name: $name,
                kind: MethodKind::$kind,
            }, )+
        ];

        impl Method {
            /// 将线上的方法字符串解析为协议枚举。
            pub fn parse(value: &str) -> Option<Self> {
                METHOD_REGISTRY
                    .iter()
                    .find(|spec| spec.name == value)
                    .map(|spec| spec.method)
            }

            /// 返回方法的 JSON-RPC 字符串。
            pub fn as_str(self) -> &'static str {
                self.spec().name
            }

            /// 返回该方法在唯一 registry 中的合同。
            pub fn spec(self) -> &'static MethodSpec {
                METHOD_REGISTRY
                    .iter()
                    .find(|spec| spec.method == self)
                    .expect("every Method variant is registered")
            }
        }
    };
}

method_registry! {
    Initialize => ("initialize", Request),
    Initialized => ("initialized", Notification),
    ThreadList => ("thread/list", Request),
    ThreadStart => ("thread/start", Request),
    ThreadSettings => ("thread/settings", Request),
    ThreadRead => ("thread/read", Request),
    SessionDelete => ("session/delete", Request),
    TurnStart => ("turn/start", Request),
    TurnSteer => ("turn/steer", Request),
    TurnFollowUp => ("turn/followUp", Request),
    ProviderStatus => ("provider/status", Request),
    TurnInterrupt => ("turn/interrupt", Request),
    ServerShutdown => ("server/shutdown", Request),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    /// 注册表只保留已收敛方法：已删除方法名必须保持不可解析。
    #[test]
    fn method_registry_keeps_only_converged_methods() {
        let names = METHOD_REGISTRY
            .iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        for expected in [
            "initialize",
            "initialized",
            "thread/list",
            "thread/start",
            "thread/settings",
            "thread/read",
            "session/delete",
            "turn/start",
            "turn/steer",
            "turn/followUp",
            "turn/interrupt",
            "provider/status",
            "server/shutdown",
        ] {
            assert!(
                names.contains(&expected),
                "missing method {expected}: {names:?}"
            );
        }
        for removed in [
            "server/capabilities",
            "thread/fork",
            "thread/archive",
            "thread/delete",
            "turn/status",
            "turn/pause",
            "turn/resume",
            "turn/input",
            "event/subscribe",
            "project/trust",
        ] {
            assert!(
                Method::parse(removed).is_none(),
                "removed method still registered: {removed}"
            );
        }
        assert_eq!(Method::TurnSteer.spec().kind, MethodKind::Request);
        assert_eq!(Method::TurnFollowUp.spec().kind, MethodKind::Request);
    }
}

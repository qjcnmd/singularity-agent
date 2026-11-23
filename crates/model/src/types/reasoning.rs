use protocol::ProviderApiProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessageMetadata {
    pub protocol: ProviderApiProtocol,
    pub provider_name: String,
    pub model_name: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl fmt::Debug for ProviderMessageMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderMessageMetadata")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl ProviderMessageMetadata {
    pub(crate) fn is_for_model(
        &self,
        provider_name: &str,
        model_name: &str,
        protocol: ProviderApiProtocol,
        endpoint: &str,
    ) -> bool {
        self.protocol == protocol
            && self.provider_name == provider_name
            && self.model_name == model_name
            && self.endpoint == endpoint
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderBlockMetadata(pub(crate) serde_json::Map<String, Value>);
impl fmt::Debug for ProviderBlockMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderBlockMetadata([redacted])")
    }
}

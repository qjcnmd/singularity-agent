//! Stable product-owned identifiers for the Singularity Windows sandbox.
//!
//! Keep account names, filesystem layout, helper names, IPC object names,
//! firewall/WFP identities, and other externally observable identifiers here.

pub const PRODUCT_NAME: &str = "Singularity Windows Sandbox";
pub const PRODUCT_VENDOR: &str = "Singularity";

pub const OFFLINE_ACCOUNT_NAME: &str = "SgSandboxOffline";
pub const ONLINE_ACCOUNT_NAME: &str = "SgSandboxOnline";
pub const SANDBOX_USERS_GROUP_NAME: &str = "SingularitySandboxUsers";
pub const SANDBOX_USERS_GROUP_COMMENT: &str = "Singularity sandbox internal group (managed)";

pub use singularity_core::PROTECTED_METADATA_DIR_NAME;
pub const SANDBOX_HOME_ENV: &str = "SINGULARITY_HOME";
pub const SANDBOX_STATE_DIR_NAME: &str = ".sandbox";
pub const SANDBOX_BIN_DIR_NAME: &str = ".sandbox-bin";
pub const SANDBOX_SECRETS_DIR_NAME: &str = ".sandbox-secrets";
pub const SANDBOX_USERS_FILE_NAME: &str = "sandbox_users.json";
pub const SETUP_MARKER_FILE_NAME: &str = "setup_marker.json";
pub const SETUP_ERROR_FILE_NAME: &str = "setup_error.json";
pub const CAPABILITY_SID_FILE_NAME: &str = "cap_sid";
pub const HELPER_BIN_DIR_NAME: &str = "bin";
pub const HELPER_RESOURCES_DIR_NAME: &str = "singularity-resources";

pub const SETUP_BINARY_NAME: &str = "singularity-windows-sandbox-setup.exe";
pub const COMMAND_RUNNER_BINARY_NAME: &str = "singularity-command-runner.exe";
pub const DEFAULT_USE_PRIVATE_DESKTOP: bool = true;
pub const PRIVATE_DESKTOP_PREFIX: &str = "SingularitySandboxDesktop";
pub const RUNNER_PIPE_PREFIX: &str = r"\\.\pipe\singularity-windows-sandbox-runner";
pub const RUNNER_CONNECT_THREAD_PREFIX: &str = "singularity-sandbox-runner-connect";
pub const READ_ACL_MUTEX_NAME: &str = r"Global\SingularitySandboxReadAcl";

pub const LOCAL_APP_DATA_VENDOR_DIR_NAME: &str = "Singularity";
pub const LOCAL_APP_DATA_PRODUCT_DIR_NAME: &str = "Agent";
pub const RUNTIME_CACHE_DIR_NAME: &str = "singularity-runtimes";
pub const NETWORK_ALLOW_LOCAL_BINDING_ENV: &str = "SINGULARITY_NETWORK_ALLOW_LOCAL_BINDING";
pub const LOG_FILE_PREFIX: &str = "singularity-windows-sandbox";

pub const FIREWALL_OFFLINE_BLOCK_RULE_NAME: &str = "singularity_sandbox_offline_block_outbound";
pub const FIREWALL_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME: &str =
    "singularity_sandbox_offline_block_loopback_tcp";
pub const FIREWALL_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME: &str =
    "singularity_sandbox_offline_block_loopback_udp";
pub const FIREWALL_OFFLINE_PROXY_ALLOW_RULE_NAME: &str =
    "singularity_sandbox_offline_allow_loopback_proxy";
pub const FIREWALL_OFFLINE_BLOCK_RULE_FRIENDLY: &str =
    "Singularity Sandbox Offline - Block Non-Loopback Outbound";
pub const FIREWALL_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY: &str =
    "Singularity Sandbox Offline - Block Loopback TCP (Except Proxy)";
pub const FIREWALL_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY: &str =
    "Singularity Sandbox Offline - Block Loopback UDP";

pub const WFP_SESSION_NAME: &str = "Singularity Windows Sandbox WFP";
pub const WFP_PROVIDER_NAME: &str = "Singularity Windows Sandbox WFP";
pub const WFP_PROVIDER_DESCRIPTION: &str =
    "Persistent WFP provider for Singularity Windows sandbox filters";
pub const WFP_SUBLAYER_NAME: &str = "Singularity Windows Sandbox WFP";
pub const WFP_SUBLAYER_DESCRIPTION: &str =
    "Persistent WFP sublayer for Singularity Windows sandbox filters";
pub const WFP_PROVIDER_KEY: u128 = 0x449e1486_077f_4da0_b515_d6453992d058;
pub const WFP_SUBLAYER_KEY: u128 = 0x5a5c1d2a_af90_40a6_a6f1_af116af4859f;

pub const WFP_FILTER_KEYS: [u128; 12] = [
    0x0fe6ec3a_f57f_42ab_beea_a76a3ab816d3,
    0xdb6157ce_5556_4fdb_97be_31d9e108ebd3,
    0xe906aa5e_7034_4509_b3c0_50440abb9543,
    0x661a90c8_1f06_4ff2_9f98_1a816f98c3f3,
    0x0dc09eab_d687_4b38_85f5_2cedc5b96315,
    0x14a4a349_44ef_483a_90e1_f6d18a099a27,
    0xb5152324_636e_47cf_8659_fbcfbf1ab93e,
    0x51e285cb_7b0a_43db_abe8_0ee3320a3064,
    0xa759c55b_a47e_40a4_88b2_bf7a085ac131,
    0x7c7b1221_fd49_40f5_b4cd_aff30959e4fb,
    0x8b0c3133_ef6b_4802_8e61_31c2becc8a7a,
    0xf3dd301c_02bc_4417_83a5_c32643ba67e0,
];

pub const WFP_FILTER_NAMES: [&str; 12] = [
    "singularity_wfp_icmp_connect_v4",
    "singularity_wfp_icmp_connect_v6",
    "singularity_wfp_icmp_assign_v4",
    "singularity_wfp_icmp_assign_v6",
    "singularity_wfp_dns_53_v4",
    "singularity_wfp_dns_53_v6",
    "singularity_wfp_dns_853_v4",
    "singularity_wfp_dns_853_v6",
    "singularity_wfp_smb_445_v4",
    "singularity_wfp_smb_445_v6",
    "singularity_wfp_smb_139_v4",
    "singularity_wfp_smb_139_v6",
];

pub const WFP_FILTER_DESCRIPTIONS: [&str; 12] = [
    "Block sandbox-account ICMP connect v4",
    "Block sandbox-account ICMP connect v6",
    "Block sandbox-account ICMP resource assignment v4",
    "Block sandbox-account ICMP resource assignment v6",
    "Block sandbox-account DNS TCP or UDP port 53 v4",
    "Block sandbox-account DNS TCP or UDP port 53 v6",
    "Block sandbox-account DNS-over-TLS port 853 v4",
    "Block sandbox-account DNS-over-TLS port 853 v6",
    "Block sandbox-account SMB port 445 v4",
    "Block sandbox-account SMB port 445 v6",
    "Block sandbox-account SMB port 139 v4",
    "Block sandbox-account SMB port 139 v6",
];

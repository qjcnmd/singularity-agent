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

/// Product-owned keys for the fixed offline-user block filters.
pub const WFP_BLOCK_FILTER_KEYS: [u128; 8] = [
    0x0fe6ec3a_f57f_42ab_beea_a76a3ab816d3,
    0xdb6157ce_5556_4fdb_97be_31d9e108ebd3,
    0xe906aa5e_7034_4509_b3c0_50440abb9543,
    0x661a90c8_1f06_4ff2_9f98_1a816f98c3f3,
    0x0dc09eab_d687_4b38_85f5_2cedc5b96315,
    0x14a4a349_44ef_483a_90e1_f6d18a099a27,
    0xb5152324_636e_47cf_8659_fbcfbf1ab93e,
    0x51e285cb_7b0a_43db_abe8_0ee3320a3064,
];

/// Product-owned keys for the broad loopback connect permits used by the
/// existing `allow_local_binding` mode.
pub const WFP_LOOPBACK_PERMIT_FILTER_KEYS: [u128; 2] = [
    0xa759c55b_a47e_40a4_88b2_bf7a085ac131,
    0x7c7b1221_fd49_40f5_b4cd_aff30959e4fb,
];

/// Fixed slots keep dynamic proxy permits bounded and make stale-port cleanup
/// deterministic.  At most ten loopback proxy ports can be present because
/// the setup payload derives them from the finite proxy environment list.
pub const WFP_PROXY_FILTER_KEYS: [u128; 20] = [
    0x8b0c3133_ef6b_4802_8e61_31c2becc8a7a,
    0xf3dd301c_02bc_4417_83a5_c32643ba67e0,
    0x12bb5dd5_57ea_4ee4_a9be_9abf6b2fd5d8,
    0x2b2e1b8c_b4f8_47b7_9ab3_b6a213983225,
    0x3af5579c_a7e1_4db0_a3a6_4f3af5b3de77,
    0x43d9c4c7_36be_48a6_a3c5_f5c8c5a0eb85,
    0x57e9ce59_2d6f_4c8d_93f9_0c1c10a16f2b,
    0x6d7b6f9d_00c3_46e0_9dc0_1e510a08ac11,
    0x75a20db5_3c91_45e4_8d22_bf2b90ccbe33,
    0x8cc6d8da_4fe0_47a8_887d_8347cf364c51,
    0x93e8748a_5c1c_4d76_a1f5_d3a8a8cc2d4e,
    0xa4a39e08_7b46_4e41_9f22_6a35f3f6642c,
    0xb4d7ce9e_8a4c_423f_9bc7_6a74e8d7a1af,
    0xc0f4cbf0_91d5_468e_a1b8_1ebfc0fba9c8,
    0xd1e1e97e_2a6b_4611_8ce6_7fcf16f7b211,
    0xe2a5f5dc_3508_4a18_8a0e_47d0a8de57b2,
    0xf1f4f08c_4e1b_4e73_a7b1_5f5d2f1d42ce,
    0x0a3d1f77_7d52_4c9e_9e19_3b1d1abf5d72,
    0x1c57eb28_90d1_4a9d_8a11_1ec4cf5a0d64,
    0x2f6d0b31_a6d2_4e06_9e52_28f2c7d0b9a4,
];

pub const WFP_BLOCK_FILTER_NAMES: [&str; 8] = [
    "singularity_wfp_auth_connect_block_v4",
    "singularity_wfp_auth_connect_block_v6",
    "singularity_wfp_resource_assignment_block_v4",
    "singularity_wfp_resource_assignment_block_v6",
    "singularity_wfp_auth_listen_block_v4",
    "singularity_wfp_auth_listen_block_v6",
    "singularity_wfp_auth_recv_accept_block_v4",
    "singularity_wfp_auth_recv_accept_block_v6",
];

pub const WFP_BLOCK_FILTER_DESCRIPTIONS: [&str; 8] = [
    "Block sandbox-account outgoing connect v4",
    "Block sandbox-account outgoing connect v6",
    "Block sandbox-account resource assignment v4",
    "Block sandbox-account resource assignment v6",
    "Block sandbox-account TCP listen v4",
    "Block sandbox-account TCP listen v6",
    "Block sandbox-account incoming accept v4",
    "Block sandbox-account incoming accept v6",
];

pub const WFP_LOOPBACK_PERMIT_FILTER_NAMES: [&str; 2] = [
    "singularity_wfp_loopback_permit_v4",
    "singularity_wfp_loopback_permit_v6",
];

pub const WFP_LOOPBACK_PERMIT_FILTER_DESCRIPTIONS: [&str; 2] = [
    "Permit sandbox-account loopback connect v4",
    "Permit sandbox-account loopback connect v6",
];

pub const WFP_PROXY_FILTER_NAME_PREFIX: &str = "singularity_wfp_proxy_connect_";
pub const WFP_PROXY_FILTER_DESCRIPTION_PREFIX: &str =
    "Permit sandbox-account loopback proxy connect ";

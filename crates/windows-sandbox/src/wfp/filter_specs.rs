use crate::product_identity::{
    WFP_BLOCK_FILTER_DESCRIPTIONS, WFP_BLOCK_FILTER_KEYS, WFP_BLOCK_FILTER_NAMES,
};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_BLOCK;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_FLAG_IS_LOOPBACK;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V6;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_LISTEN_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_LISTEN_V6;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6;
use windows_sys::core::GUID;

/// WFP condition values used by the product-owned filters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConditionSpec {
    User,
    Protocol(u8),
    RemotePort(u16),
    FlagsAllSet(u32),
}

/// Fixed filter metadata.  Proxy permits are assembled at setup time from the
/// finite proxy-port list and use the same representation in `wfp.rs`.
#[derive(Clone, Copy)]
pub(super) struct FilterSpec {
    pub(super) key: GUID,
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    pub(super) layer_key: GUID,
    pub(super) conditions: &'static [ConditionSpec],
    pub(super) action: u32,
    pub(super) weight: u64,
}

/// Explicit weights make the proxy/loopback permits win only within this
/// product sublayer, while the user-scoped block filters remain hard blocks
/// against lower-priority providers.
pub(super) const BLOCK_FILTER_WEIGHT: u64 = 0x1000;
pub(super) const PERMIT_FILTER_WEIGHT: u64 = 0xffff;

const USER_ONLY: &[ConditionSpec] = &[ConditionSpec::User];

pub(super) const FILTER_SPECS: &[FilterSpec] = &[
    FilterSpec {
        key: GUID::from_u128(WFP_BLOCK_FILTER_KEYS[0]),
        name: WFP_BLOCK_FILTER_NAMES[0],
        description: WFP_BLOCK_FILTER_DESCRIPTIONS[0],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: USER_ONLY,
        action: FWP_ACTION_BLOCK,
        weight: BLOCK_FILTER_WEIGHT,
    },
    FilterSpec {
        key: GUID::from_u128(WFP_BLOCK_FILTER_KEYS[1]),
        name: WFP_BLOCK_FILTER_NAMES[1],
        description: WFP_BLOCK_FILTER_DESCRIPTIONS[1],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: USER_ONLY,
        action: FWP_ACTION_BLOCK,
        weight: BLOCK_FILTER_WEIGHT,
    },
    FilterSpec {
        key: GUID::from_u128(WFP_BLOCK_FILTER_KEYS[2]),
        name: WFP_BLOCK_FILTER_NAMES[2],
        description: WFP_BLOCK_FILTER_DESCRIPTIONS[2],
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
        conditions: USER_ONLY,
        action: FWP_ACTION_BLOCK,
        weight: BLOCK_FILTER_WEIGHT,
    },
    FilterSpec {
        key: GUID::from_u128(WFP_BLOCK_FILTER_KEYS[3]),
        name: WFP_BLOCK_FILTER_NAMES[3],
        description: WFP_BLOCK_FILTER_DESCRIPTIONS[3],
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
        conditions: USER_ONLY,
        action: FWP_ACTION_BLOCK,
        weight: BLOCK_FILTER_WEIGHT,
    },
    FilterSpec {
        key: GUID::from_u128(WFP_BLOCK_FILTER_KEYS[4]),
        name: WFP_BLOCK_FILTER_NAMES[4],
        description: WFP_BLOCK_FILTER_DESCRIPTIONS[4],
        layer_key: FWPM_LAYER_ALE_AUTH_LISTEN_V4,
        conditions: USER_ONLY,
        action: FWP_ACTION_BLOCK,
        weight: BLOCK_FILTER_WEIGHT,
    },
    FilterSpec {
        key: GUID::from_u128(WFP_BLOCK_FILTER_KEYS[5]),
        name: WFP_BLOCK_FILTER_NAMES[5],
        description: WFP_BLOCK_FILTER_DESCRIPTIONS[5],
        layer_key: FWPM_LAYER_ALE_AUTH_LISTEN_V6,
        conditions: USER_ONLY,
        action: FWP_ACTION_BLOCK,
        weight: BLOCK_FILTER_WEIGHT,
    },
    FilterSpec {
        key: GUID::from_u128(WFP_BLOCK_FILTER_KEYS[6]),
        name: WFP_BLOCK_FILTER_NAMES[6],
        description: WFP_BLOCK_FILTER_DESCRIPTIONS[6],
        layer_key: FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
        conditions: USER_ONLY,
        action: FWP_ACTION_BLOCK,
        weight: BLOCK_FILTER_WEIGHT,
    },
    FilterSpec {
        key: GUID::from_u128(WFP_BLOCK_FILTER_KEYS[7]),
        name: WFP_BLOCK_FILTER_NAMES[7],
        description: WFP_BLOCK_FILTER_DESCRIPTIONS[7],
        layer_key: FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
        conditions: USER_ONLY,
        action: FWP_ACTION_BLOCK,
        weight: BLOCK_FILTER_WEIGHT,
    },
];

pub(super) fn loopback_condition() -> ConditionSpec {
    ConditionSpec::FlagsAllSet(FWP_CONDITION_FLAG_IS_LOOPBACK)
}

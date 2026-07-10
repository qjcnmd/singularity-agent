use crate::product_identity::{WFP_FILTER_DESCRIPTIONS, WFP_FILTER_KEYS, WFP_FILTER_NAMES};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V6;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6;
use windows_sys::Win32::Networking::WinSock::IPPROTO_ICMP;
use windows_sys::Win32::Networking::WinSock::IPPROTO_ICMPV6;
use windows_sys::core::GUID;

#[derive(Clone, Copy)]
pub(super) enum ConditionSpec {
    User,
    Protocol(u8),
    RemotePort(u16),
}

#[derive(Clone, Copy)]
pub(super) struct FilterSpec {
    pub(super) key: GUID,
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    pub(super) layer_key: GUID,
    pub(super) conditions: &'static [ConditionSpec],
}

pub(super) const FILTER_SPECS: &[FilterSpec] = &[
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[0]),
        name: WFP_FILTER_NAMES[0],
        description: WFP_FILTER_DESCRIPTIONS[0],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[1]),
        name: WFP_FILTER_NAMES[1],
        description: WFP_FILTER_DESCRIPTIONS[1],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[2]),
        name: WFP_FILTER_NAMES[2],
        description: WFP_FILTER_DESCRIPTIONS[2],
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[3]),
        name: WFP_FILTER_NAMES[3],
        description: WFP_FILTER_DESCRIPTIONS[3],
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[4]),
        name: WFP_FILTER_NAMES[4],
        description: WFP_FILTER_DESCRIPTIONS[4],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[5]),
        name: WFP_FILTER_NAMES[5],
        description: WFP_FILTER_DESCRIPTIONS[5],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[6]),
        name: WFP_FILTER_NAMES[6],
        description: WFP_FILTER_DESCRIPTIONS[6],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[7]),
        name: WFP_FILTER_NAMES[7],
        description: WFP_FILTER_DESCRIPTIONS[7],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[8]),
        name: WFP_FILTER_NAMES[8],
        description: WFP_FILTER_DESCRIPTIONS[8],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[9]),
        name: WFP_FILTER_NAMES[9],
        description: WFP_FILTER_DESCRIPTIONS[9],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[10]),
        name: WFP_FILTER_NAMES[10],
        description: WFP_FILTER_DESCRIPTIONS[10],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
    FilterSpec {
        key: GUID::from_u128(WFP_FILTER_KEYS[11]),
        name: WFP_FILTER_NAMES[11],
        description: WFP_FILTER_DESCRIPTIONS[11],
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
];

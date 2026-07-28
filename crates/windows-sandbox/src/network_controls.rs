//! Read-only verification of the persistent controls used by the offline identity.
//!
//! Setup owns creating these controls.  The runtime only compares the product-owned
//! Firewall rules and WFP filters immediately before a child is spawned; the setup
//! marker is never treated as proof that those controls still exist.

use crate::product_identity::{
    FIREWALL_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME, FIREWALL_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME,
    FIREWALL_OFFLINE_BLOCK_RULE_NAME, FIREWALL_OFFLINE_PROXY_ALLOW_RULE_NAME,
};
use anyhow::{Context, Result};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, RPC_E_CHANGED_MODE, VARIANT_TRUE};
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, INetFwRule3, NET_FW_ACTION, NET_FW_ACTION_BLOCK, NET_FW_IP_PROTOCOL_ANY,
    NET_FW_IP_PROTOCOL_TCP, NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE2_ALL, NET_FW_RULE_DIR_OUT,
    NET_FW_RULE_DIRECTION, NetFwPolicy2,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::{BSTR, Interface};

/// Loopback address range used by both setup and runtime drift verification.
pub const LOOPBACK_REMOTE_ADDRESSES: &str = "127.0.0.0/8,::/127";
/// Non-loopback address range covered by the offline outbound block rule.
pub const NON_LOOPBACK_REMOTE_ADDRESSES: &str = "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::,::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";

#[derive(Debug, Clone, PartialEq, Eq)]
struct FirewallRuleSnapshot {
    enabled: bool,
    direction: NET_FW_RULE_DIRECTION,
    action: NET_FW_ACTION,
    profiles: i32,
    protocol: i32,
    remote_addresses: String,
    remote_ports: Option<String>,
    local_user_scope: String,
}

#[derive(Clone, Copy)]
struct FirewallRuleExpectation<'a> {
    name: &'static str,
    protocol: i32,
    remote_addresses: &'static str,
    remote_ports: Option<&'a str>,
}

struct ComApartment {
    uninitialize: bool,
}

impl ComApartment {
    fn enter() -> Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() {
            return Ok(Self { uninitialize: true });
        }
        // The caller may already be running inside an initialized STA.  COM still permits
        // read-only Firewall queries in that apartment, but this call must not be unbalanced.
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self {
                uninitialize: false,
            });
        }
        Err(anyhow::anyhow!("CoInitializeEx failed: {result:?}"))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

/// Returns whether all persistent controls for the selected offline identity are current.
///
/// This function is deliberately read-only.  A false result is a stale setup signal and lets
/// the existing elevated setup path reconcile the controls; a query error is propagated so the
/// caller cannot turn an unverifiable state into strict execution.
pub(crate) fn offline_network_controls_are_current(
    offline_sid: &[u8],
    offline_sid_string: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<bool> {
    Ok(
        firewall_rules_are_current(offline_sid_string, proxy_ports, allow_local_binding)?
            && crate::wfp::installed_filter_set_is_current(
                offline_sid,
                proxy_ports,
                allow_local_binding,
            )?,
    )
}

fn firewall_rules_are_current(
    offline_sid: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<bool> {
    let _apartment = ComApartment::enter()?;
    let policy: INetFwPolicy2 = unsafe {
        CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
            .context("create Windows Firewall policy")?
    };
    let rules = unsafe { policy.Rules() }.context("read Windows Firewall rules")?;
    let blocked_tcp_ports =
        blocked_loopback_tcp_remote_ports(proxy_ports).unwrap_or_else(|| "*".to_string());
    let mut expected = vec![FirewallRuleExpectation {
        name: FIREWALL_OFFLINE_BLOCK_RULE_NAME,
        protocol: NET_FW_IP_PROTOCOL_ANY.0,
        remote_addresses: NON_LOOPBACK_REMOTE_ADDRESSES,
        remote_ports: None,
    }];
    if !allow_local_binding {
        expected.extend([
            FirewallRuleExpectation {
                name: FIREWALL_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME,
                protocol: NET_FW_IP_PROTOCOL_TCP.0,
                remote_addresses: LOOPBACK_REMOTE_ADDRESSES,
                remote_ports: Some(&blocked_tcp_ports),
            },
            FirewallRuleExpectation {
                name: FIREWALL_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME,
                protocol: NET_FW_IP_PROTOCOL_UDP.0,
                remote_addresses: LOOPBACK_REMOTE_ADDRESSES,
                remote_ports: Some("*"),
            },
        ]);
    }

    for expectation in expected {
        let Some(rule) = firewall_rule(&rules, expectation.name)? else {
            return Ok(false);
        };
        let snapshot = firewall_rule_snapshot(&rule, expectation.remote_ports.is_some())
            .with_context(|| format!("inspect Windows Firewall rule {}", expectation.name))?;
        if !firewall_rule_matches(
            &snapshot,
            expectation.protocol,
            expectation.remote_addresses,
            expectation.remote_ports,
            offline_sid,
        ) {
            return Ok(false);
        }
    }

    // There is no proxy allow rule in the current fail-closed contract.  In local-binding mode
    // the two loopback blocks are intentionally absent; any stale copy is drift, not success.
    if firewall_rule(&rules, FIREWALL_OFFLINE_PROXY_ALLOW_RULE_NAME)?.is_some() {
        return Ok(false);
    }
    if allow_local_binding
        && (firewall_rule(&rules, FIREWALL_OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME)?.is_some()
            || firewall_rule(&rules, FIREWALL_OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME)?.is_some())
    {
        return Ok(false);
    }
    Ok(true)
}

fn firewall_rule(
    rules: &windows::Win32::NetworkManagement::WindowsFirewall::INetFwRules,
    name: &str,
) -> Result<Option<INetFwRule3>> {
    match unsafe { rules.Item(&BSTR::from(name)) } {
        Ok(rule) => rule
            .cast::<INetFwRule3>()
            .with_context(|| format!("read Windows Firewall rule {name}"))
            .map(Some),
        Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read Windows Firewall rule {name}")),
    }
}

fn firewall_rule_snapshot(
    rule: &INetFwRule3,
    include_remote_ports: bool,
) -> Result<FirewallRuleSnapshot> {
    Ok(FirewallRuleSnapshot {
        enabled: unsafe { rule.Enabled() }.context("read Enabled")? == VARIANT_TRUE,
        direction: unsafe { rule.Direction() }.context("read Direction")?,
        action: unsafe { rule.Action() }.context("read Action")?,
        profiles: unsafe { rule.Profiles() }.context("read Profiles")?,
        protocol: unsafe { rule.Protocol() }.context("read Protocol")?,
        remote_addresses: unsafe { rule.RemoteAddresses() }
            .context("read RemoteAddresses")?
            .to_string(),
        remote_ports: include_remote_ports
            .then(|| unsafe { rule.RemotePorts() })
            .transpose()
            .context("read RemotePorts")?
            .map(|value| value.to_string()),
        local_user_scope: unsafe { rule.LocalUserAuthorizedList() }
            .context("read LocalUserAuthorizedList")?
            .to_string(),
    })
}

fn firewall_rule_matches(
    rule: &FirewallRuleSnapshot,
    expected_protocol: i32,
    expected_remote_addresses: &str,
    expected_remote_ports: Option<&str>,
    offline_sid: &str,
) -> bool {
    rule.enabled
        && rule.direction == NET_FW_RULE_DIR_OUT
        && rule.action == NET_FW_ACTION_BLOCK
        && rule.profiles == NET_FW_PROFILE2_ALL.0
        && rule.protocol == expected_protocol
        && normalized_firewall_addresses(&rule.remote_addresses)
            == normalized_firewall_addresses(expected_remote_addresses)
        && rule.remote_ports.as_deref() == expected_remote_ports
        && rule.local_user_scope == format!("O:LSD:(A;;CC;;;{offline_sid})")
}

fn normalized_firewall_addresses(addresses: &str) -> Option<Vec<String>> {
    let mut normalized = addresses
        .split(',')
        .map(str::trim)
        .map(normalized_firewall_address)
        .collect::<Option<Vec<_>>>()?;
    normalized.sort_unstable();
    normalized.dedup();
    Some(normalized)
}

fn normalized_firewall_address(address: &str) -> Option<String> {
    if address.is_empty() {
        return None;
    }
    if let Some((start, end)) = address.split_once('-')
        && start == end
    {
        return Some(start.to_string());
    }
    let Some((network, mask)) = address.split_once('/') else {
        return Some(address.to_string());
    };
    if !mask.contains('.') {
        return Some(address.to_string());
    }
    let mask = mask.parse::<std::net::Ipv4Addr>().ok()?;
    let bits = u32::from(mask);
    let prefix = bits.leading_ones();
    let expected = if prefix == 0 {
        0
    } else {
        u32::MAX << (u32::BITS - prefix)
    };
    (bits == expected).then(|| format!("{network}/{prefix}"))
}

/// Returns the TCP port complement blocked when selected loopback proxy ports remain usable.
pub fn blocked_loopback_tcp_remote_ports(proxy_ports: &[u16]) -> Option<String> {
    let mut allowed_ports = proxy_ports
        .iter()
        .copied()
        .filter(|port| *port != 0)
        .collect::<Vec<_>>();
    allowed_ports.sort_unstable();
    allowed_ports.dedup();

    let mut blocked_ranges = Vec::new();
    let mut start = 1_u32;
    for port in allowed_ports {
        let port = u32::from(port);
        if port < start {
            continue;
        }
        if port > start {
            blocked_ranges.push(port_range_string(start, port - 1));
        }
        start = port + 1;
    }
    if start <= u32::from(u16::MAX) {
        blocked_ranges.push(port_range_string(start, u32::from(u16::MAX)));
    }
    (!blocked_ranges.is_empty()).then(|| blocked_ranges.join(","))
}

fn port_range_string(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

#[cfg(test)]
mod tests {
    use super::{FirewallRuleSnapshot, firewall_rule_matches, normalized_firewall_addresses};
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        NET_FW_ACTION_BLOCK, NET_FW_IP_PROTOCOL_ANY, NET_FW_PROFILE2_ALL, NET_FW_RULE_DIR_OUT,
    };

    #[test]
    fn drifted_firewall_rule_never_proves_offline_controls() {
        let mut rule = FirewallRuleSnapshot {
            enabled: true,
            direction: NET_FW_RULE_DIR_OUT,
            action: NET_FW_ACTION_BLOCK,
            profiles: NET_FW_PROFILE2_ALL.0,
            protocol: NET_FW_IP_PROTOCOL_ANY.0,
            remote_addresses: "normalized".to_string(),
            remote_ports: None,
            local_user_scope: "O:LSD:(A;;CC;;;S-1-5-21-42)".to_string(),
        };
        assert!(firewall_rule_matches(
            &rule,
            NET_FW_IP_PROTOCOL_ANY.0,
            "normalized",
            None,
            "S-1-5-21-42"
        ));

        rule.enabled = false;
        assert!(!firewall_rule_matches(
            &rule,
            NET_FW_IP_PROTOCOL_ANY.0,
            "normalized",
            None,
            "S-1-5-21-42"
        ));
    }

    #[test]
    fn firewall_address_com_readback_preserves_the_configured_semantics() {
        assert_eq!(
            normalized_firewall_addresses("127.0.0.0/255.0.0.0,::/127"),
            normalized_firewall_addresses("127.0.0.0/8,::/127")
        );
        assert_eq!(
            normalized_firewall_addresses(
                "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::-::,::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
            ),
            normalized_firewall_addresses(
                "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::,::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
            )
        );
        assert_ne!(
            normalized_firewall_addresses("127.0.0.0/255.255.0.0,::/127"),
            normalized_firewall_addresses("127.0.0.0/8,::/127")
        );
    }
}

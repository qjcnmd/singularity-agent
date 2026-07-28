mod filter_specs;

use crate::product_identity::{
    WFP_BLOCK_FILTER_KEYS, WFP_LOOPBACK_PERMIT_FILTER_DESCRIPTIONS,
    WFP_LOOPBACK_PERMIT_FILTER_KEYS, WFP_LOOPBACK_PERMIT_FILTER_NAMES,
    WFP_PROVIDER_DESCRIPTION as PROVIDER_DESCRIPTION, WFP_PROVIDER_KEY,
    WFP_PROVIDER_NAME as PROVIDER_NAME, WFP_PROXY_FILTER_DESCRIPTION_PREFIX, WFP_PROXY_FILTER_KEYS,
    WFP_PROXY_FILTER_NAME_PREFIX, WFP_SESSION_NAME as SESSION_NAME,
    WFP_SUBLAYER_DESCRIPTION as SUBLAYER_DESCRIPTION, WFP_SUBLAYER_KEY,
    WFP_SUBLAYER_NAME as SUBLAYER_NAME,
};
use crate::to_wide;
use anyhow::{Result, bail};
use std::ffi::{OsStr, c_void};
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::Foundation::FWP_E_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::FWP_E_FILTER_NOT_FOUND;
use windows_sys::Win32::Foundation::FWP_E_NOT_FOUND;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_PERMIT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTRL_MATCH_FILTER;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_BYTE_BLOB;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_VALUE0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_VALUE0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_EMPTY;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_MATCH_EQUAL;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_MATCH_FLAGS_ALL_SET;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_SECURITY_DESCRIPTOR_TYPE;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT8;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT16;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT32;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT64;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_VALUE0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTRL_READ;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_ALE_USER_ID;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_FLAGS;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_IP_PROTOCOL;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_IP_REMOTE_PORT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_DISPLAY_DATA0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER_CONDITION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_PROVIDER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_PROVIDER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SESSION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SUBLAYER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SUBLAYER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmEngineClose0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmEngineOpen0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterDeleteByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterGetByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterGetSecurityInfoByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterSetSecurityInfoByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFreeMemory0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmProviderAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmSubLayerAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionAbort0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionBegin0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionCommit0;
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
use windows_sys::Win32::Security::AclSizeInformation;
use windows_sys::Win32::Security::Authorization::BuildExplicitAccessWithNameW;
use windows_sys::Win32::Security::Authorization::BuildSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GRANT_ACCESS;
use windows_sys::Win32::Security::Authorization::SET_ACCESS;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::GetAclInformation;
use windows_sys::Win32::Security::GetSecurityDescriptorDacl;
use windows_sys::Win32::Security::IsValidSecurityDescriptor;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_DEFAULT;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::core::GUID;

use filter_specs::ConditionSpec;
use filter_specs::FILTER_SPECS;
use filter_specs::PERMIT_FILTER_WEIGHT;
use filter_specs::loopback_condition;

const PROVIDER_KEY: GUID = GUID::from_u128(WFP_PROVIDER_KEY);
const SUBLAYER_KEY: GUID = GUID::from_u128(WFP_SUBLAYER_KEY);
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const PROXY_FILTERS_PER_PORT: usize = 2;
const MAX_PROXY_PORTS: usize = WFP_PROXY_FILTER_KEYS.len() / PROXY_FILTERS_PER_PORT;

/// The metadata needed to install or verify one product-owned WFP filter.
struct ExpectedFilter {
    key: GUID,
    name: String,
    description: String,
    layer_key: GUID,
    conditions: Vec<ConditionSpec>,
    action: u32,
    weight: u64,
}

/// Installs the persistent, user-scoped WFP policy and grants `reader_account`
/// only filter metadata read access used by runtime drift checks.
///
/// The policy blocks outgoing connects, bind/resource assignment, TCP listen,
/// and incoming accept for the offline identity.  A configured connect
/// exception keeps prerequisite local-address assignment available, then only
/// permits explicit loopback proxy ports (or the existing broad loopback
/// exception when `allow_local_binding` is true).  All filters are
/// product-owned and persistent.
pub(crate) fn install_wfp_filters_for_account(
    offline_account: &str,
    reader_account: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<usize> {
    let expected = expected_filters(proxy_ports, allow_local_binding)?;
    let engine = Engine::open()?;
    let mut transaction = engine.begin_transaction()?;
    ensure_provider(engine.handle)?;
    ensure_sublayer(engine.handle)?;

    // Delete every fixed product key before re-adding the current policy.  The
    // bounded key set also removes stale proxy-port slots from previous setup.
    for key in all_product_filter_keys() {
        delete_filter_if_present(engine.handle, &key)?;
    }

    let user_condition = UserMatchCondition::for_account(offline_account)?;
    for spec in &expected {
        add_filter(engine.handle, spec, &user_condition)?;
    }
    transaction.commit()?;
    drop(transaction);

    for spec in &expected {
        grant_filter_read_access(engine.handle, &spec.key, reader_account)?;
    }
    Ok(expected.len())
}

/// Verifies the complete product-owned filter set without mutating WFP state.
///
/// Missing, extra, or altered fixed-slot filters return `Ok(false)`, so a
/// stale setup marker cannot select the offline identity.  A query error is
/// propagated and therefore remains fail-closed at the caller.
pub(crate) fn installed_filter_set_is_current(
    offline_sid: &[u8],
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<bool> {
    let expected = expected_filters(proxy_ports, allow_local_binding)?;
    let expected_keys = expected.iter().map(|spec| spec.key).collect::<Vec<_>>();
    let engine = Engine::open()?;

    for spec in &expected {
        let Some(filter) = get_filter(engine.handle, &spec.key)? else {
            return Ok(false);
        };
        if !filter_matches_expected(filter.as_ref(), spec, offline_sid) {
            return Ok(false);
        }
    }
    for key in all_product_filter_keys() {
        if expected_keys.iter().any(|expected| guid_eq(expected, &key)) {
            continue;
        }
        if get_filter(engine.handle, &key)?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn expected_filters(proxy_ports: &[u16], allow_local_binding: bool) -> Result<Vec<ExpectedFilter>> {
    let mut ports = proxy_ports.to_vec();
    ports.sort_unstable();
    ports.dedup();
    if ports.contains(&0) {
        bail!("WFP proxy port 0 is not a valid loopback proxy port");
    }
    if ports.len() > MAX_PROXY_PORTS {
        bail!("WFP proxy port list exceeds the bounded filter-slot capacity");
    }

    let has_connect_exception = allow_local_binding || !ports.is_empty();
    let mut expected = FILTER_SPECS
        .iter()
        // A client connect needs Windows to assign a local address and ephemeral port before
        // AUTH_CONNECT can apply the exact loopback exception.  LISTEN and RECV_ACCEPT remain
        // blocked, so omitting these two prerequisite blocks does not permit inbound traffic.
        .filter(|spec| !has_connect_exception || !is_resource_assignment_layer(&spec.layer_key))
        .map(|spec| ExpectedFilter {
            key: spec.key,
            name: spec.name.to_string(),
            description: spec.description.to_string(),
            layer_key: spec.layer_key,
            conditions: spec.conditions.to_vec(),
            action: spec.action,
            weight: spec.weight,
        })
        .collect::<Vec<_>>();

    if allow_local_binding {
        for family in 0..2 {
            expected.push(ExpectedFilter {
                key: GUID::from_u128(WFP_LOOPBACK_PERMIT_FILTER_KEYS[family]),
                name: WFP_LOOPBACK_PERMIT_FILTER_NAMES[family].to_string(),
                description: WFP_LOOPBACK_PERMIT_FILTER_DESCRIPTIONS[family].to_string(),
                layer_key: auth_connect_layer(family),
                conditions: vec![ConditionSpec::User, loopback_condition()],
                action: FWP_ACTION_PERMIT,
                weight: PERMIT_FILTER_WEIGHT,
            });
        }
    } else {
        for (slot, port) in ports.iter().copied().enumerate() {
            for family in 0..2 {
                let key_index = slot * PROXY_FILTERS_PER_PORT + family;
                let ip_version = if family == 0 { 4 } else { 6 };
                expected.push(ExpectedFilter {
                    key: GUID::from_u128(WFP_PROXY_FILTER_KEYS[key_index]),
                    name: format!(
                        "{WFP_PROXY_FILTER_NAME_PREFIX}v{}_slot{slot:02}",
                        ip_version
                    ),
                    description: format!(
                        "{WFP_PROXY_FILTER_DESCRIPTION_PREFIX}v{} port {port}",
                        ip_version
                    ),
                    layer_key: auth_connect_layer(family),
                    conditions: vec![
                        ConditionSpec::User,
                        loopback_condition(),
                        ConditionSpec::Protocol(6),
                        ConditionSpec::RemotePort(port),
                    ],
                    action: FWP_ACTION_PERMIT,
                    weight: PERMIT_FILTER_WEIGHT,
                });
            }
        }
    }
    Ok(expected)
}

fn auth_connect_layer(family: usize) -> GUID {
    if family == 0 {
        windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V4
    } else {
        windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V6
    }
}

fn is_resource_assignment_layer(layer: &GUID) -> bool {
    guid_eq(
        layer,
        &windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
    ) || guid_eq(
        layer,
        &windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
    )
}

fn all_product_filter_keys() -> Vec<GUID> {
    let mut keys = Vec::with_capacity(
        WFP_BLOCK_FILTER_KEYS.len()
            + WFP_LOOPBACK_PERMIT_FILTER_KEYS.len()
            + WFP_PROXY_FILTER_KEYS.len(),
    );
    keys.extend(WFP_BLOCK_FILTER_KEYS.into_iter().map(GUID::from_u128));
    keys.extend(
        WFP_LOOPBACK_PERMIT_FILTER_KEYS
            .into_iter()
            .map(GUID::from_u128),
    );
    keys.extend(WFP_PROXY_FILTER_KEYS.into_iter().map(GUID::from_u128));
    keys
}

struct OwnedFilter(*mut FWPM_FILTER0);

impl OwnedFilter {
    fn as_ref(&self) -> &FWPM_FILTER0 {
        // SAFETY: `FwpmFilterGetByKey0` returned a non-null WFP-owned allocation.
        unsafe { &*self.0 }
    }
}

impl Drop for OwnedFilter {
    fn drop(&mut self) {
        let mut allocation = self.0.cast::<c_void>();
        // SAFETY: WFP requires `FwpmFreeMemory0` for memory returned by `GetByKey`.
        unsafe { FwpmFreeMemory0(&mut allocation) };
    }
}

fn get_filter(engine: HANDLE, key: &GUID) -> Result<Option<OwnedFilter>> {
    let mut filter = null_mut();
    let result = unsafe { FwpmFilterGetByKey0(engine, key, &mut filter) };
    if matches!(result, value if value == FWP_E_FILTER_NOT_FOUND as u32
        || value == FWP_E_NOT_FOUND as u32
        || value == ERROR_ACCESS_DENIED)
    {
        return Ok(None);
    }
    ensure_success(result, "FwpmFilterGetByKey0")?;
    if filter.is_null() {
        bail!("FwpmFilterGetByKey0 returned a null filter");
    }
    Ok(Some(OwnedFilter(filter)))
}

/// Grants the reader only WFP filter-object metadata access needed by drift checks.
fn grant_filter_read_access(engine: HANDLE, key: &GUID, reader_account: &str) -> Result<()> {
    let mut current_dacl: *mut ACL = null_mut();
    let mut security_descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let get_result = unsafe {
        FwpmFilterGetSecurityInfoByKey0(
            engine,
            key,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut current_dacl,
            null_mut(),
            &mut security_descriptor,
        )
    };
    if let Err(error) = ensure_success(get_result, "FwpmFilterGetSecurityInfoByKey0") {
        if !security_descriptor.is_null() {
            let mut allocation = security_descriptor.cast::<c_void>();
            unsafe { FwpmFreeMemory0(&mut allocation) };
        }
        return Err(error);
    }
    if security_descriptor.is_null() {
        bail!("FwpmFilterGetSecurityInfoByKey0 returned a null security descriptor");
    }

    let reader_account = to_wide(OsStr::new(reader_account));
    let mut access: EXPLICIT_ACCESS_W = unsafe { zeroed() };
    unsafe {
        BuildExplicitAccessWithNameW(
            &mut access,
            reader_account.as_ptr(),
            FWPM_ACTRL_READ,
            SET_ACCESS,
            0,
        );
    }
    let mut updated_dacl: *mut ACL = null_mut();
    let result = (|| -> Result<()> {
        let set_entries_result =
            unsafe { SetEntriesInAclW(1, &access, current_dacl, &mut updated_dacl) };
        ensure_success(set_entries_result, "SetEntriesInAclW(WFP filter)")?;
        let set_result = unsafe {
            FwpmFilterSetSecurityInfoByKey0(
                engine,
                key,
                DACL_SECURITY_INFORMATION,
                null(),
                null(),
                updated_dacl,
                null(),
            )
        };
        ensure_success(set_result, "FwpmFilterSetSecurityInfoByKey0")
    })();

    if !updated_dacl.is_null() {
        unsafe { LocalFree(updated_dacl as HLOCAL) };
    }
    let mut allocation = security_descriptor.cast::<c_void>();
    unsafe { FwpmFreeMemory0(&mut allocation) };
    result
}

fn filter_matches_expected(
    filter: &FWPM_FILTER0,
    expected: &ExpectedFilter,
    offline_sid: &[u8],
) -> bool {
    guid_eq(&filter.filterKey, &expected.key)
        && filter.flags == FWPM_FILTER_FLAG_PERSISTENT
        && wide_string_equals(filter.displayData.name, &expected.name)
        && wide_string_equals(filter.displayData.description, &expected.description)
        && !filter.providerKey.is_null()
        // SAFETY: a non-null provider key belongs to the returned WFP filter allocation.
        && unsafe { guid_eq(&*filter.providerKey, &PROVIDER_KEY) }
        && guid_eq(&filter.layerKey, &expected.layer_key)
        && guid_eq(&filter.subLayerKey, &SUBLAYER_KEY)
        && filter.action.r#type == expected.action
        && filter_weight_matches(filter.weight, expected.weight)
        && filter_conditions_match(filter, &expected.conditions, offline_sid)
}

fn filter_weight_matches(weight: FWP_VALUE0, expected: u64) -> bool {
    if weight.r#type != FWP_UINT64 {
        return false;
    }
    let pointer = unsafe { weight.Anonymous.uint64 };
    !pointer.is_null() && unsafe { *pointer == expected }
}

fn wide_string_equals(value: *mut u16, expected: &str) -> bool {
    if value.is_null() {
        return false;
    }
    let mut length = 0;
    while unsafe { *value.add(length) } != 0 {
        length += 1;
        if length > 4096 {
            return false;
        }
    }
    let value = unsafe { std::slice::from_raw_parts(value, length) };
    String::from_utf16(value).ok().as_deref() == Some(expected)
}

fn filter_conditions_match(
    filter: &FWPM_FILTER0,
    expected: &[ConditionSpec],
    offline_sid: &[u8],
) -> bool {
    if filter.numFilterConditions != expected.len() as u32
        || (filter.numFilterConditions != 0 && filter.filterCondition.is_null())
    {
        return false;
    }
    let actual = if filter.numFilterConditions == 0 {
        &[][..]
    } else {
        // SAFETY: WFP returned `numFilterConditions` entries in this allocation.
        unsafe {
            std::slice::from_raw_parts(filter.filterCondition, filter.numFilterConditions as usize)
        }
    };
    expected.iter().all(|expected| {
        actual
            .iter()
            .any(|actual| condition_matches(actual, expected, offline_sid))
    })
}

fn condition_matches(
    actual: &FWPM_FILTER_CONDITION0,
    expected: &ConditionSpec,
    offline_sid: &[u8],
) -> bool {
    match expected {
        ConditionSpec::User => {
            guid_eq(&actual.fieldKey, &FWPM_CONDITION_ALE_USER_ID)
                && actual.matchType == FWP_MATCH_EQUAL
                && actual.conditionValue.r#type == FWP_SECURITY_DESCRIPTOR_TYPE
                && user_condition_matches(actual.conditionValue, offline_sid)
        }
        ConditionSpec::Protocol(protocol) => {
            guid_eq(&actual.fieldKey, &FWPM_CONDITION_IP_PROTOCOL)
                && actual.matchType == FWP_MATCH_EQUAL
                && actual.conditionValue.r#type == FWP_UINT8
                && unsafe { actual.conditionValue.Anonymous.uint8 == *protocol }
        }
        ConditionSpec::RemotePort(port) => {
            guid_eq(&actual.fieldKey, &FWPM_CONDITION_IP_REMOTE_PORT)
                && actual.matchType == FWP_MATCH_EQUAL
                && actual.conditionValue.r#type == FWP_UINT16
                && unsafe { actual.conditionValue.Anonymous.uint16 == *port }
        }
        ConditionSpec::FlagsAllSet(flags) => {
            guid_eq(&actual.fieldKey, &FWPM_CONDITION_FLAGS)
                && actual.matchType == FWP_MATCH_FLAGS_ALL_SET
                && actual.conditionValue.r#type == FWP_UINT32
                && unsafe { actual.conditionValue.Anonymous.uint32 == *flags }
        }
    }
}

fn user_condition_matches(value: FWP_CONDITION_VALUE0, offline_sid: &[u8]) -> bool {
    let blob = unsafe { value.Anonymous.sd };
    if blob.is_null() || offline_sid.is_empty() {
        return false;
    }
    let blob = unsafe { &*blob };
    if blob.size == 0
        || blob.data.is_null()
        || unsafe { IsValidSecurityDescriptor(blob.data.cast()) } == 0
    {
        return false;
    }

    let mut dacl_present = 0;
    let mut dacl_defaulted = 0;
    let mut dacl: *mut ACL = null_mut();
    if unsafe {
        GetSecurityDescriptorDacl(
            blob.data.cast(),
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
        || dacl_present == 0
        || dacl.is_null()
    {
        return false;
    }

    let mut info: ACL_SIZE_INFORMATION = unsafe { zeroed() };
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || info.AceCount != 1
    {
        return false;
    }

    let mut ace = null_mut();
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
        return false;
    }
    let ace = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
    ace.Header.AceType == ACCESS_ALLOWED_ACE_TYPE
        && ace.Header.AceFlags == 0
        && ace.Mask == FWP_ACTRL_MATCH_FILTER
        && unsafe {
            EqualSid(
                (&ace.SidStart as *const u32).cast_mut().cast(),
                offline_sid.as_ptr().cast_mut().cast(),
            ) != 0
        }
}

fn guid_eq(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

/// Owns an open WFP engine handle and closes it on drop.
struct Engine {
    handle: HANDLE,
}

impl Engine {
    fn open() -> Result<Self> {
        let session_name = to_wide(OsStr::new(SESSION_NAME));
        let mut session: FWPM_SESSION0 = unsafe { zeroed() };
        session.displayData = FWPM_DISPLAY_DATA0 {
            name: session_name.as_ptr() as *mut _,
            description: null_mut(),
        };
        session.txnWaitTimeoutInMSec = INFINITE;

        let mut handle = HANDLE::default();
        let result = unsafe {
            FwpmEngineOpen0(
                null(),
                RPC_C_AUTHN_DEFAULT as u32,
                null(),
                &session,
                &mut handle,
            )
        };
        ensure_success(result, "FwpmEngineOpen0")?;
        Ok(Self { handle })
    }

    fn begin_transaction(&self) -> Result<Transaction<'_>> {
        let result = unsafe { FwpmTransactionBegin0(self.handle, 0) };
        ensure_success(result, "FwpmTransactionBegin0")?;
        Ok(Transaction {
            engine: self,
            committed: false,
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.handle);
        }
    }
}

/// Aborts an open WFP transaction unless it was explicitly committed.
struct Transaction<'a> {
    engine: &'a Engine,
    committed: bool,
}

impl Transaction<'_> {
    fn commit(&mut self) -> Result<()> {
        let result = unsafe { FwpmTransactionCommit0(self.engine.handle) };
        ensure_success(result, "FwpmTransactionCommit0")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                FwpmTransactionAbort0(self.engine.handle);
            }
        }
    }
}

/// Builds the ALE_USER_ID condition blob that scopes filters to one account.
struct UserMatchCondition {
    security_descriptor: PSECURITY_DESCRIPTOR,
    blob: FWP_BYTE_BLOB,
}

impl UserMatchCondition {
    fn for_account(account: &str) -> Result<Self> {
        let account_w = to_wide(OsStr::new(account));
        let mut access: EXPLICIT_ACCESS_W = unsafe { zeroed() };
        unsafe {
            BuildExplicitAccessWithNameW(
                &mut access,
                account_w.as_ptr(),
                FWP_ACTRL_MATCH_FILTER,
                GRANT_ACCESS,
                0,
            );
        }

        let mut security_descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let mut security_descriptor_len = 0;
        let result = unsafe {
            BuildSecurityDescriptorW(
                null(),
                null(),
                1,
                &access,
                0,
                null(),
                null_mut(),
                &mut security_descriptor_len,
                &mut security_descriptor,
            )
        };
        ensure_success(result, "BuildSecurityDescriptorW")?;

        Ok(Self {
            security_descriptor,
            blob: FWP_BYTE_BLOB {
                size: security_descriptor_len,
                data: security_descriptor as *mut u8,
            },
        })
    }
}

impl Drop for UserMatchCondition {
    fn drop(&mut self) {
        if !self.security_descriptor.is_null() {
            unsafe {
                LocalFree(self.security_descriptor as HLOCAL);
            }
        }
    }
}

/// Ensures the persistent Singularity WFP provider exists.
fn ensure_provider(engine: HANDLE) -> Result<()> {
    let provider_name = to_wide(OsStr::new(PROVIDER_NAME));
    let provider_description = to_wide(OsStr::new(PROVIDER_DESCRIPTION));
    let provider = FWPM_PROVIDER0 {
        providerKey: PROVIDER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: provider_name.as_ptr() as *mut _,
            description: provider_description.as_ptr() as *mut _,
        },
        flags: FWPM_PROVIDER_FLAG_PERSISTENT,
        providerData: empty_blob(),
        serviceName: null_mut(),
    };

    let result = unsafe { FwpmProviderAdd0(engine, &provider, null_mut()) };
    ensure_success_or(result, "FwpmProviderAdd0", &[FWP_E_ALREADY_EXISTS as u32])
}

/// Ensures the persistent Singularity sublayer exists under the Singularity provider.
fn ensure_sublayer(engine: HANDLE) -> Result<()> {
    let sublayer_name = to_wide(OsStr::new(SUBLAYER_NAME));
    let sublayer_description = to_wide(OsStr::new(SUBLAYER_DESCRIPTION));
    let provider_key = PROVIDER_KEY;
    let sublayer = FWPM_SUBLAYER0 {
        subLayerKey: SUBLAYER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: sublayer_name.as_ptr() as *mut _,
            description: sublayer_description.as_ptr() as *mut _,
        },
        flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const _ as *mut _,
        providerData: empty_blob(),
        weight: 0x8000,
    };

    let result = unsafe { FwpmSubLayerAdd0(engine, &sublayer, null_mut()) };
    ensure_success_or(result, "FwpmSubLayerAdd0", &[FWP_E_ALREADY_EXISTS as u32])
}

/// Adds one product-owned WFP filter with explicit action and weight.
fn add_filter(
    engine: HANDLE,
    spec: &ExpectedFilter,
    user_condition: &UserMatchCondition,
) -> Result<()> {
    let filter_name = to_wide(OsStr::new(&spec.name));
    let filter_description = to_wide(OsStr::new(&spec.description));
    let mut filter_conditions = build_conditions(&spec.conditions, user_condition);
    let provider_key = PROVIDER_KEY;
    let weight = spec.weight;
    let filter = FWPM_FILTER0 {
        filterKey: spec.key,
        displayData: FWPM_DISPLAY_DATA0 {
            name: filter_name.as_ptr() as *mut _,
            description: filter_description.as_ptr() as *mut _,
        },
        flags: FWPM_FILTER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const _ as *mut _,
        providerData: empty_blob(),
        layerKey: spec.layer_key,
        subLayerKey: SUBLAYER_KEY,
        weight: FWP_VALUE0 {
            r#type: FWP_UINT64,
            Anonymous:
                windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_VALUE0_0 {
                    uint64: &weight as *const _ as *mut _,
                },
        },
        numFilterConditions: filter_conditions.len() as u32,
        filterCondition: filter_conditions.as_mut_ptr(),
        action: FWPM_ACTION0 {
            r#type: spec.action,
            Anonymous: FWPM_ACTION0_0 {
                filterType: zero_guid(),
            },
        },
        Anonymous: FWPM_FILTER0_0 { rawContext: 0 },
        reserved: null_mut(),
        filterId: 0,
        effectiveWeight: empty_value(),
    };

    let mut filter_id = 0_u64;
    let result = unsafe { FwpmFilterAdd0(engine, &filter, null_mut(), &mut filter_id) };
    ensure_success(result, &format!("FwpmFilterAdd0({})", spec.name))
}

/// Converts compact condition specs into WFP filter conditions.
fn build_conditions(
    specs: &[ConditionSpec],
    user_condition: &UserMatchCondition,
) -> Vec<FWPM_FILTER_CONDITION0> {
    specs
        .iter()
        .map(|spec| match spec {
            ConditionSpec::User => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_ALE_USER_ID,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        sd: &user_condition.blob as *const _ as *mut _,
                    },
                },
            },
            ConditionSpec::Protocol(protocol) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_PROTOCOL,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT8,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint8: *protocol },
                },
            },
            ConditionSpec::RemotePort(port) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT16,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint16: *port },
                },
            },
            ConditionSpec::FlagsAllSet(flags) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_FLAGS,
                matchType: FWP_MATCH_FLAGS_ALL_SET,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT32,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint32: *flags },
                },
            },
        })
        .collect()
}

/// Deletes one product filter before re-adding it.
fn delete_filter_if_present(engine: HANDLE, key: &GUID) -> Result<()> {
    let result = unsafe { FwpmFilterDeleteByKey0(engine, key) };
    ensure_success_or(
        result,
        "FwpmFilterDeleteByKey0",
        &[FWP_E_FILTER_NOT_FOUND as u32, FWP_E_NOT_FOUND as u32],
    )
}

fn ensure_success(result: u32, operation: &str) -> Result<()> {
    ensure_success_or(result, operation, &[])
}

fn ensure_success_or(result: u32, operation: &str, allowed: &[u32]) -> Result<()> {
    if result == 0 || allowed.contains(&result) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "{operation} failed: {}",
            format_error_code(result)
        ))
    }
}

fn format_error_code(result: u32) -> String {
    format!("0x{result:08X}")
}

fn empty_blob() -> FWP_BYTE_BLOB {
    FWP_BYTE_BLOB {
        size: 0,
        data: null_mut(),
    }
}

fn empty_value() -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_EMPTY,
        Anonymous: unsafe { zeroed() },
    }
}

fn zero_guid() -> GUID {
    GUID::from_u128(0)
}

#[cfg(test)]
mod tests {
    use super::filter_specs::ConditionSpec;
    use super::{FILTER_SPECS, all_product_filter_keys, expected_filters, guid_eq};
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_BLOCK;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_PERMIT;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_FLAG_IS_LOOPBACK;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_LISTEN_V4;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4;

    #[test]
    fn product_filter_keys_and_fixed_block_names_are_unique() {
        let keys = all_product_filter_keys()
            .into_iter()
            .map(|key| (key.data1, key.data2, key.data3, key.data4))
            .collect::<BTreeSet<_>>();
        assert_eq!(keys.len(), all_product_filter_keys().len());

        let names = FILTER_SPECS
            .iter()
            .map(|spec| spec.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), FILTER_SPECS.len());
    }

    #[test]
    fn network_denied_has_no_loopback_permit_without_proxy_or_local_binding() {
        let filters = expected_filters(&[], false).expect("valid empty proxy list");
        assert!(
            filters
                .iter()
                .all(|filter| filter.action == FWP_ACTION_BLOCK)
        );
        assert!(
            filters.iter().any(|filter| {
                guid_eq(&filter.layer_key, &FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4)
            })
        );
    }

    #[test]
    fn proxy_permits_are_loopback_tcp_and_explicit_remote_ports() {
        let filters = expected_filters(&[8080, 8080, 1081], false).expect("valid proxy list");
        let permits = filters
            .iter()
            .filter(|filter| filter.action == FWP_ACTION_PERMIT)
            .collect::<Vec<_>>();
        assert_eq!(permits.len(), 4);
        assert!(permits.iter().any(|filter| filter.name.contains("_v4_")));
        assert!(permits.iter().any(|filter| filter.name.contains("_v6_")));
        assert!(permits.iter().all(|filter| !filter.name.contains("_v5_")));
        assert!(permits.iter().all(|filter| {
            filter.conditions.contains(&ConditionSpec::User)
                && filter
                    .conditions
                    .contains(&ConditionSpec::FlagsAllSet(FWP_CONDITION_FLAG_IS_LOOPBACK))
                && filter.conditions.contains(&ConditionSpec::Protocol(6))
                && filter.conditions.iter().any(|condition| {
                    matches!(
                        condition,
                        ConditionSpec::RemotePort(1081) | ConditionSpec::RemotePort(8080)
                    )
                })
        }));
        assert!(
            !filters.iter().any(|filter| {
                guid_eq(&filter.layer_key, &FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4)
            })
        );
        assert!(filters.iter().any(|filter| guid_eq(
            &filter.layer_key,
            &FWPM_LAYER_ALE_AUTH_LISTEN_V4
        ) && filter.action == FWP_ACTION_BLOCK));
    }

    #[test]
    fn local_binding_mode_takes_precedence_over_detected_proxy_ports() {
        let filters = expected_filters(&[8080], true).expect("valid proxy list");
        let permits = filters
            .iter()
            .filter(|filter| filter.action == FWP_ACTION_PERMIT)
            .collect::<Vec<_>>();
        assert_eq!(permits.len(), 2);
        assert!(permits.iter().all(|filter| {
            filter.conditions
                == [
                    ConditionSpec::User,
                    ConditionSpec::FlagsAllSet(FWP_CONDITION_FLAG_IS_LOOPBACK),
                ]
        }));
        assert!(
            !filters.iter().any(|filter| {
                guid_eq(&filter.layer_key, &FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4)
            })
        );
        assert!(filters.iter().any(|filter| {
            guid_eq(&filter.layer_key, &FWPM_LAYER_ALE_AUTH_LISTEN_V4)
                && filter.action == FWP_ACTION_BLOCK
        }));
    }

    #[test]
    fn proxy_port_order_does_not_change_fixed_filter_keys() {
        let left = expected_filters(&[1081, 8080], false).expect("valid proxy list");
        let right = expected_filters(&[8080, 1081], false).expect("valid proxy list");
        let left = left
            .iter()
            .filter(|filter| filter.action == FWP_ACTION_PERMIT)
            .map(|filter| (filter.key, filter.conditions.clone()))
            .collect::<Vec<_>>();
        let right = right
            .iter()
            .filter(|filter| filter.action == FWP_ACTION_PERMIT)
            .map(|filter| (filter.key, filter.conditions.clone()))
            .collect::<Vec<_>>();
        assert_eq!(left.len(), right.len());
        for ((left_key, left_conditions), (right_key, right_conditions)) in left.iter().zip(&right)
        {
            assert!(guid_eq(left_key, right_key));
            assert_eq!(left_conditions, right_conditions);
        }
    }

    #[test]
    fn too_many_proxy_ports_fail_closed_before_wfp_mutation() {
        let ports = (1..=11).collect::<Vec<_>>();
        assert!(expected_filters(&ports, false).is_err());
    }
}

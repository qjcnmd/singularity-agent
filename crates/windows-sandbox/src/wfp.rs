mod filter_specs;

use crate::product_identity::{
    WFP_PROVIDER_DESCRIPTION as PROVIDER_DESCRIPTION, WFP_PROVIDER_KEY,
    WFP_PROVIDER_NAME as PROVIDER_NAME, WFP_SESSION_NAME as SESSION_NAME,
    WFP_SUBLAYER_DESCRIPTION as SUBLAYER_DESCRIPTION, WFP_SUBLAYER_KEY,
    WFP_SUBLAYER_NAME as SUBLAYER_NAME,
};
use crate::to_wide;
use anyhow::Result;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::mem::zeroed;
use std::ptr::null;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::Foundation::FWP_E_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::FWP_E_FILTER_NOT_FOUND;
use windows_sys::Win32::Foundation::FWP_E_NOT_FOUND;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_BLOCK;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTRL_MATCH_FILTER;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_BYTE_BLOB;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_VALUE0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_VALUE0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_EMPTY;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_MATCH_EQUAL;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_SECURITY_DESCRIPTOR_TYPE;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT8;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT16;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_VALUE0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTRL_READ;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_ALE_USER_ID;
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
use filter_specs::FilterSpec;

const PROVIDER_KEY: GUID = GUID::from_u128(WFP_PROVIDER_KEY);
const SUBLAYER_KEY: GUID = GUID::from_u128(WFP_SUBLAYER_KEY);
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// Installs the persistent Singularity WFP filters for `offline_account` and
/// grants `reader_account` exact read-only access to their metadata.
///
/// This is intended to run from the already-elevated setup helper. Callers
/// must treat any returned error as a fail-closed setup failure.
pub(crate) fn install_wfp_filters_for_account(
    offline_account: &str,
    reader_account: &str,
) -> Result<usize> {
    let engine = Engine::open()?;
    let mut transaction = engine.begin_transaction()?;
    ensure_provider(engine.handle)?;
    ensure_sublayer(engine.handle)?;

    let user_condition = UserMatchCondition::for_account(offline_account)?;
    let mut installed_filter_count = 0;
    for spec in FILTER_SPECS {
        delete_filter_if_present(engine.handle, &spec.key)?;
        add_filter(engine.handle, spec, &user_condition)?;
        installed_filter_count += 1;
    }

    transaction.commit()?;
    drop(transaction);
    for spec in FILTER_SPECS {
        grant_filter_read_access(engine.handle, &spec.key, reader_account)?;
    }
    Ok(installed_filter_count)
}

/// Verifies that every product-owned persistent WFP filter is still present
/// with the enforcement metadata established by setup.
///
/// The query is read-only and is used before selecting the offline sandbox
/// identity. Missing or altered filters return `Ok(false)`. Access denied is
/// also stale so an installation created before read-only ACLs can migrate via
/// elevated setup; other query failures remain errors and fail closed.
pub(crate) fn installed_filter_set_is_current(offline_sid: &[u8]) -> Result<bool> {
    let engine = Engine::open()?;
    for spec in FILTER_SPECS {
        let Some(filter) = get_filter(engine.handle, &spec.key)? else {
            return Ok(false);
        };
        if !filter_matches_spec(filter.as_ref(), spec, offline_sid) {
            return Ok(false);
        }
    }
    Ok(true)
}

struct OwnedFilter(*mut FWPM_FILTER0);

impl OwnedFilter {
    fn as_ref(&self) -> &FWPM_FILTER0 {
        // SAFETY: `FwpmFilterGetByKey0` returned a non-null filter owned by this wrapper.
        unsafe { &*self.0 }
    }
}

impl Drop for OwnedFilter {
    fn drop(&mut self) {
        let mut allocation = self.0.cast::<c_void>();
        // SAFETY: WFP allocated this object and requires `FwpmFreeMemory0` to release it.
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
        anyhow::bail!("FwpmFilterGetByKey0 returned a null filter");
    }
    Ok(Some(OwnedFilter(filter)))
}

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
    ensure_success(get_result, "FwpmFilterGetSecurityInfoByKey0")?;
    if security_descriptor.is_null() {
        anyhow::bail!("FwpmFilterGetSecurityInfoByKey0 returned a null security descriptor");
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
    let mut allocation = security_descriptor;
    unsafe { FwpmFreeMemory0(&mut allocation) };
    result
}

fn filter_matches_spec(filter: &FWPM_FILTER0, spec: &FilterSpec, offline_sid: &[u8]) -> bool {
    guid_eq(&filter.filterKey, &spec.key)
        && filter.flags & FWPM_FILTER_FLAG_PERSISTENT != 0
        && !filter.providerKey.is_null()
        // SAFETY: a non-null `providerKey` belongs to the returned WFP filter allocation.
        && unsafe { guid_eq(&*filter.providerKey, &PROVIDER_KEY) }
        && guid_eq(&filter.layerKey, &spec.layer_key)
        && guid_eq(&filter.subLayerKey, &SUBLAYER_KEY)
        && filter.action.r#type == FWP_ACTION_BLOCK
        && filter_conditions_match(filter, spec.conditions, offline_sid)
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
    let actual = unsafe {
        std::slice::from_raw_parts(filter.filterCondition, filter.numFilterConditions as usize)
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
    if actual.matchType != FWP_MATCH_EQUAL {
        return false;
    }
    match expected {
        ConditionSpec::User => {
            guid_eq(&actual.fieldKey, &FWPM_CONDITION_ALE_USER_ID)
                && actual.conditionValue.r#type == FWP_SECURITY_DESCRIPTOR_TYPE
                && user_condition_matches(actual.conditionValue, offline_sid)
        }
        ConditionSpec::Protocol(protocol) => {
            guid_eq(&actual.fieldKey, &FWPM_CONDITION_IP_PROTOCOL)
                && actual.conditionValue.r#type == FWP_UINT8
                && unsafe { actual.conditionValue.Anonymous.uint8 == *protocol }
        }
        ConditionSpec::RemotePort(port) => {
            guid_eq(&actual.fieldKey, &FWPM_CONDITION_IP_REMOTE_PORT)
                && actual.conditionValue.r#type == FWP_UINT16
                && unsafe { actual.conditionValue.Anonymous.uint16 == *port }
        }
    }
}

fn user_condition_matches(value: FWP_CONDITION_VALUE0, offline_sid: &[u8]) -> bool {
    let blob = unsafe { value.Anonymous.sd };
    if blob.is_null() || offline_sid.is_empty() {
        return false;
    }
    let blob = unsafe { &*blob };
    if blob.data.is_null()
        || blob.size == 0
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

/// Adds one blocking WFP filter from the static filter spec list.
fn add_filter(
    engine: HANDLE,
    spec: &FilterSpec,
    user_condition: &UserMatchCondition,
) -> Result<()> {
    let filter_name = to_wide(OsStr::new(spec.name));
    let filter_description = to_wide(OsStr::new(spec.description));
    let mut filter_conditions = build_conditions(spec.conditions, user_condition);
    let provider_key = PROVIDER_KEY;
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
        weight: empty_value(),
        numFilterConditions: filter_conditions.len() as u32,
        filterCondition: filter_conditions.as_mut_ptr(),
        action: FWPM_ACTION0 {
            r#type: FWP_ACTION_BLOCK,
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

/// Converts our compact condition specs into WFP filter conditions.
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
        })
        .collect()
}

/// Deletes an old copy of a filter before re-adding it.
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
    use super::FILTER_SPECS;
    use super::PROVIDER_KEY;
    use super::SUBLAYER_KEY;
    use super::UserMatchCondition;
    use super::build_conditions;
    use super::filter_matches_spec;
    use super::installed_filter_set_is_current;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;
    use std::ptr::null_mut;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_BLOCK;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_EMPTY;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_VALUE0;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0_0;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER_FLAG_PERSISTENT;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER0;
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER0_0;
    use windows_sys::core::GUID;

    #[test]
    fn filter_keys_are_unique() {
        let keys = FILTER_SPECS
            .iter()
            .map(|spec| {
                (
                    spec.key.data1,
                    spec.key.data2,
                    spec.key.data3,
                    spec.key.data4,
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(keys.len(), FILTER_SPECS.len());
    }

    #[test]
    fn filter_names_are_unique() {
        let names = FILTER_SPECS
            .iter()
            .map(|spec| spec.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), FILTER_SPECS.len());
    }

    #[test]
    fn filter_readiness_requires_product_enforcement_metadata() {
        let spec = &FILTER_SPECS[0];
        let mut provider_key = PROVIDER_KEY;
        let mut filter: FWPM_FILTER0 = unsafe { std::mem::zeroed() };
        filter.filterKey = spec.key;
        filter.flags = FWPM_FILTER_FLAG_PERSISTENT;
        filter.providerKey = &mut provider_key;
        filter.layerKey = spec.layer_key;
        filter.subLayerKey = SUBLAYER_KEY;
        filter.numFilterConditions = spec.conditions.len() as u32;
        let user_condition = UserMatchCondition::for_account("Everyone")
            .expect("build deterministic user condition");
        let offline_sid =
            crate::winutil::resolve_sid("Everyone").expect("resolve deterministic SID");
        let mut conditions = build_conditions(spec.conditions, &user_condition);
        filter.filterCondition = conditions.as_mut_ptr();
        filter.action = FWPM_ACTION0 {
            r#type: FWP_ACTION_BLOCK,
            Anonymous: FWPM_ACTION0_0 {
                filterType: GUID::from_u128(0),
            },
        };
        filter.Anonymous = FWPM_FILTER0_0 { rawContext: 0 };
        filter.effectiveWeight = FWP_VALUE0 {
            r#type: FWP_EMPTY,
            Anonymous: unsafe { std::mem::zeroed() },
        };

        assert!(filter_matches_spec(&filter, spec, &offline_sid));
        let original_field_key = conditions[1].fieldKey;
        conditions[1].fieldKey = GUID::from_u128(0);
        assert!(!filter_matches_spec(&filter, spec, &offline_sid));
        conditions[1].fieldKey = original_field_key;
        filter.providerKey = null_mut();
        assert!(!filter_matches_spec(&filter, spec, &offline_sid));
    }

    #[test]
    fn filter_readiness_query_is_non_mutating() {
        let offline_sid =
            crate::winutil::resolve_sid("Everyone").expect("resolve deterministic SID");
        installed_filter_set_is_current(&offline_sid).expect("query WFP filter readiness");
    }
}

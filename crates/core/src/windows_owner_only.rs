//! Windows owner-only ACL primitive.
//!
//! This is the single repository implementation for the model/store security
//! layers. It pins one object by handle, verifies the current-user owner and a
//! protected single-ACE DACL, and applies the same contract when repair is safe.
//!
//! The owner of an object is implicitly granted READ_CONTROL and WRITE_DAC by
//! the access check even when the DACL does not name it, so tightening only
//! ever writes the protected DACL through handles opened with those rights.
//! WRITE_OWNER is never requested: the current user must already be the owner
//! (it created the object), otherwise tightening fails closed instead of
//! taking ownership of a foreign object.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetSecurityInfo, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
    SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, GENERIC_MAPPING, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const MAX_TOKEN_INFORMATION_BYTES: usize = 64 * 1024;

/// Verify that the exact object pinned by `file` has the owner-only contract.
pub fn ensure_owner_only_handle(file: &File) -> io::Result<()> {
    let current_sid = current_user_sid()?;
    let handle = file.as_raw_handle() as HANDLE;
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS || descriptor.is_null() {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor as HLOCAL) };
        }
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let result = inspect_descriptor(descriptor, owner, dacl, &current_sid);
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
}

/// Apply the protected owner-only DACL through the handle, then verify it.
///
/// The owner is never rewritten: the caller must have created the object (and
/// therefore already owns it), and the handle must be opened with
/// `READ_CONTROL|WRITE_DAC`, which the implicit owner rights always grant.
pub fn set_owner_only_handle(file: &File) -> io::Result<()> {
    apply_owner_only_security(file)?;
    ensure_owner_only_handle(file)
}

/// Create a new file already restricted to the owner-only contract.
pub fn create_owner_only_file(path: &Path) -> io::Result<File> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            0,
        )
    };
    if handle == -1 || handle == 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle as _) };
    set_owner_only_handle(&file)?;
    Ok(file)
}

/// Verify a file path, repairing an inherited/incorrect DACL in place.
///
/// Repair only touches the DACL and only when the object is already owned by
/// the current user; a foreign-owned path fails closed.
pub fn ensure_owner_only_file(path: &Path) -> io::Result<()> {
    ensure_owner_only_path(path, false)
}

/// Verify a directory path, repairing under the same owner contract as files.
pub fn ensure_owner_only_dir(path: &Path) -> io::Result<()> {
    ensure_owner_only_path(path, true)
}

fn ensure_owner_only_path(path: &Path, directory: bool) -> io::Result<()> {
    // 先用读类权限验证：已合规路径（包括被其他句柄以无写共享方式持有）不需要
    // 写类句柄，与 File::open 语义一致；不合规才进入修复。
    let verify = open_security_path(path, directory, FILE_GENERIC_READ)
        .map_err(|error| io::Error::other(format!("verify {path:?}: {error}")))?;
    if ensure_owner_only_handle(&verify).is_ok() {
        return Ok(());
    }
    // 修复序列（owner 预检查 → 写 protected DACL → 写后复核）在同一个句柄上
    // 执行，钉住同一对象：外部 owner 在任何 ACL 修改前 fail closed。
    let repair =
        open_security_path(path, directory, READ_CONTROL | WRITE_DAC).map_err(|error| {
            io::Error::other(format!(
                "open owner repair handle for {}: {error}",
                path.display()
            ))
        })?;
    if !handle_owner_is_current_user(&repair)? {
        return Err(io::Error::other(format!(
            "path {} is owned by another user; refusing to tighten",
            path.display()
        )));
    }
    set_owner_only_handle(&repair).map_err(|error| {
        io::Error::other(format!("set owner-only ACL on {}: {error}", path.display()))
    })
}

/// Open the object pinned by `path` for security inspection/repair. The flags
/// pin reparse points themselves instead of following them.
fn open_security_path(path: &Path, directory: bool, access: u32) -> io::Result<File> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = if directory {
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
    } else {
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT
    };
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            flags,
            0,
        )
    };
    if handle == -1 || handle == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

fn apply_owner_only_security(file: &File) -> io::Result<()> {
    let mut sid = current_user_sid()?;
    let owner_sid = sid.as_mut_ptr() as *mut c_void;
    let trustee = TRUSTEE_W {
        pMultipleTrustee: null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: owner_sid as *mut u16,
    };
    let entry = EXPLICIT_ACCESS_W {
        // DELETE 是 session/delete 与备份清理所需：Windows rename/remove 需要
        // 目标文件 ACL 显式授予 DELETE；FILE_GENERIC_WRITE 并不包含该权限。
        grfAccessPermissions: FILE_GENERIC_READ
            | FILE_GENERIC_WRITE
            | DELETE
            | READ_CONTROL
            | WRITE_DAC,
        grfAccessMode: SET_ACCESS,
        grfInheritance: 0,
        Trustee: trustee,
    };
    let mut dacl: *mut ACL = null_mut();
    let status = unsafe { SetEntriesInAclW(1, &entry, null(), &mut dacl) };
    if status != ERROR_SUCCESS || dacl.is_null() {
        if !dacl.is_null() {
            unsafe { LocalFree(dacl as HLOCAL) };
        }
        return Err(io::Error::other("owner-only ACL could not be created"));
    }
    let handle = file.as_raw_handle() as HANDLE;
    let status = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    unsafe { LocalFree(dacl as HLOCAL) };
    if status != ERROR_SUCCESS {
        return Err(io::Error::other("owner-only ACL could not be applied"));
    }
    Ok(())
}

/// 读取 handle 固定对象的 owner SID 并与当前进程用户比较。
fn handle_owner_is_current_user(file: &File) -> io::Result<bool> {
    let current_sid = current_user_sid()?;
    let handle = file.as_raw_handle() as HANDLE;
    let mut owner = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS || descriptor.is_null() || owner.is_null() {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor as HLOCAL) };
        }
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let matches = unsafe { EqualSid(owner, current_sid.as_ptr() as *mut c_void) } != 0;
    unsafe { LocalFree(descriptor as HLOCAL) };
    Ok(matches)
}

fn inspect_descriptor(
    descriptor: *mut c_void,
    owner: *mut c_void,
    dacl: *mut ACL,
    current_sid: &[u8],
) -> io::Result<()> {
    if owner.is_null() || current_sid.is_empty() {
        return Err(io::Error::other("path is not owner-only"));
    }
    let current_sid = current_sid.as_ptr() as *mut c_void;
    if unsafe { EqualSid(owner, current_sid) } == 0 || dacl.is_null() {
        return Err(io::Error::other("path is not owner-only"));
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
        || control & SE_DACL_PROTECTED == 0
    {
        return Err(io::Error::other("path DACL is not protected"));
    }
    let mut dacl_present = 0;
    let mut descriptor_dacl = null_mut();
    let mut dacl_defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &mut dacl_present,
            &mut descriptor_dacl,
            &mut dacl_defaulted,
        )
    } == 0
        || dacl_present == 0
        || descriptor_dacl.is_null()
        || descriptor_dacl != dacl
    {
        return Err(io::Error::other("path DACL is not present"));
    }
    let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        GetAclInformation(
            dacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || info.AceCount != 1
    {
        return Err(io::Error::other("path DACL must contain exactly one ACE"));
    }
    let mut ace = null_mut();
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
        return Err(io::Error::other("path DACL has no ACE"));
    }
    let header = unsafe { &*(ace as *const ACE_HEADER) };
    if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags != 0 {
        return Err(io::Error::other(
            "path DACL ACE is not a non-inherited allow ACE",
        ));
    }
    let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
    let sid = &allowed.SidStart as *const u32 as *mut c_void;
    if unsafe { EqualSid(sid, current_sid) } == 0 {
        return Err(io::Error::other("path DACL ACE is not the current user"));
    }
    let mut mask = allowed.Mask;
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
    };
    unsafe { windows_sys::Win32::Security::MapGenericMask(&mut mask, &mapping) };
    if (mask & (FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE))
        != (FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
    {
        return Err(io::Error::other(
            "path DACL ACE does not grant the required read/write/delete access",
        ));
    }
    Ok(())
}

fn current_user_sid() -> io::Result<Vec<u8>> {
    let mut token: HANDLE = 0;
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let result = current_user_sid_from_token(token);
    unsafe { CloseHandle(token) };
    result
}

fn current_user_sid_from_token(token: HANDLE) -> io::Result<Vec<u8>> {
    let mut length = 0;
    let _ = unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length) };
    let length = usize::try_from(length)
        .map_err(|_| io::Error::other("owner-only ACL could not be checked"))?;
    if length == 0 || length > MAX_TOKEN_INFORMATION_BYTES {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let mut buffer = vec![0u8; length];
    let mut return_length = length as u32;
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut c_void,
            length as u32,
            &mut return_length,
        )
    } == 0
    {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let token_user = unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER) };
    if token_user.User.Sid.is_null() {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let sid_length = unsafe { GetLengthSid(token_user.User.Sid) } as usize;
    if sid_length == 0 {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    Ok(
        unsafe {
            std::slice::from_raw_parts(token_user.User.Sid as *const u8, sid_length).to_vec()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    // accctrl.h 的 INHERIT_FLAGS 值；windows-sys 0.52 未导出，测试内固定。
    const SUB_OBJECTS_INHERIT: u32 = 0x1;
    const SUB_CONTAINERS_INHERIT: u32 = 0x2;

    /// 用「当前用户仅 Modify（无 WRITE_DAC/WRITE_OWNER）」的 protected DACL 重现
    /// 共享 TEMP 目录（如 D:\Temp）的默认继承结果。
    fn apply_modify_only_dacl(file: &File, inheritable: bool) {
        let mut sid = current_user_sid().expect("current user sid");
        let owner_sid = sid.as_mut_ptr() as *mut c_void;
        let trustee = TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: owner_sid as *mut u16,
        };
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_GENERIC_READ
                | FILE_GENERIC_WRITE
                | FILE_GENERIC_EXECUTE
                | DELETE,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if inheritable {
                SUB_CONTAINERS_INHERIT | SUB_OBJECTS_INHERIT
            } else {
                0
            },
            Trustee: trustee,
        };
        let mut dacl: *mut ACL = null_mut();
        let status = unsafe { SetEntriesInAclW(1, &entry, null(), &mut dacl) };
        assert_eq!(status, ERROR_SUCCESS, "build modify-only DACL");
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        unsafe { LocalFree(dacl as HLOCAL) };
        assert_eq!(status, ERROR_SUCCESS, "apply modify-only DACL");
    }

    #[test]
    fn modify_only_owned_file_is_tightened_without_write_owner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("modify-only.jsonl");
        std::fs::write(&path, "{}").expect("write file");
        let handle =
            open_security_path(&path, false, READ_CONTROL | WRITE_DAC).expect("open for fixture");
        apply_modify_only_dacl(&handle, false);
        drop(handle);

        ensure_owner_only_file(&path)
            .expect("owner-implied WRITE_DAC must tighten a modify-only DACL");
        let verify = File::open(&path).expect("reopen");
        ensure_owner_only_handle(&verify).expect("owner + protected DACL verification");
    }

    #[test]
    fn modify_only_owned_dir_is_tightened_without_write_owner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir(&sessions).expect("create dir");
        let handle = open_security_path(&sessions, true, READ_CONTROL | WRITE_DAC)
            .expect("open dir for fixture");
        apply_modify_only_dacl(&handle, false);
        drop(handle);

        ensure_owner_only_dir(&sessions)
            .expect("owner-implied WRITE_DAC must tighten a modify-only dir");
        ensure_owner_only_dir(&sessions).expect("tightened dir reopens clean");
    }

    #[test]
    fn create_owner_only_file_works_under_modify_only_parent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let handle = open_security_path(dir.path(), true, READ_CONTROL | WRITE_DAC)
            .expect("open parent for fixture");
        apply_modify_only_dacl(&handle, true);
        drop(handle);

        let rollout = dir.path().join("session.jsonl");
        let file = create_owner_only_file(&rollout)
            .expect("session rollout creation under a modify-only parent");
        ensure_owner_only_handle(&file).expect("new rollout is owner-only");
    }

    #[test]
    fn foreign_owned_path_fails_closed() {
        // 系统文件 owner 是 TrustedInstaller/Administrators 而非当前用户：
        // 非提升时修复句柄打开即被拒，提升时走 owner 不匹配分支；两条路径都
        // 必须失败且绝不改写系统对象的 ACL。
        let system = Path::new(r"C:\Windows\System32\kernel32.dll");
        if std::fs::symlink_metadata(system).is_ok() {
            assert!(
                ensure_owner_only_file(system).is_err(),
                "foreign-owned path must fail closed"
            );
        }
    }

    #[test]
    fn compliant_path_passes_while_another_handle_denies_write_sharing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("held.jsonl");
        std::fs::write(&path, "{}").expect("write file");
        ensure_owner_only_file(&path).expect("tighten first");

        let holder = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .expect("hold without write sharing");
        ensure_owner_only_file(&path)
            .expect("already-compliant path must not need a write-class handle");
        drop(holder);
    }
}

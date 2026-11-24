//! Windows 秘密文件与目录的受限 ACL（创建即受保护、读取前校验）。
//!
//! 只在 Windows 编译。Unix 继续使用 0700/0600。这里的失败一律向上抛，
//! 不允许把「无法校验」降级成空账本或重新生成密钥。

use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetSecurityDescriptorControl, GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::services::dsh_control::error::{codes, DshError};

fn state_error(context: &str, error: impl std::fmt::Display) -> DshError {
    DshError::new(codes::IPC_STATE, format!("{context}: {error}"))
}

struct OwnedToken(HANDLE);
impl Drop for OwnedToken {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 当前进程 token 的字符串 SID（唯一 ACL 授权主体来源，不按用户名猜测）。
pub fn current_sid() -> Result<String, DshError> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| state_error("open current process token", e))?;
    let token = OwnedToken(token);
    let mut size = 0u32;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut size) };
    if size < size_of::<TOKEN_USER>() as u32 || size > 1024 * 1024 {
        return Err(DshError::new(
            codes::IPC_STATE,
            "invalid current user token size",
        ));
    }
    let words = (size as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; words];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
    }
    .map_err(|e| state_error("read current user token", e))?;
    let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let mut sid = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) }
        .map_err(|e| state_error("convert current user SID", e))?;
    let result = unsafe { sid.to_string() }.map_err(|e| state_error("decode current user SID", e));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
    }
    result
}

fn wide(path: &std::path::Path) -> Result<Vec<u16>, DshError> {
    let mut out: Vec<u16> = path.as_os_str().encode_wide().collect();
    if out.is_empty() || out.contains(&0) {
        return Err(DshError::new(codes::IPC_STATE, "invalid windows path"));
    }
    out.push(0);
    Ok(out)
}

/// 受保护 SDDL：owner + 仅当前用户与 LOCAL SYSTEM 完全控制，禁止继承。
fn protected_sddl(sid: &str, directory: bool) -> Vec<u16> {
    let inheritance = if directory { "OICI" } else { "" };
    format!("O:{sid}D:P(A;{inheritance};FA;;;SY)(A;{inheritance};FA;;;{sid})")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

fn security_attributes(sddl: &[u16]) -> Result<(Descriptor, SECURITY_ATTRIBUTES), DshError> {
    let mut raw = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut raw,
            None,
        )
    }
    .map_err(|e| state_error("create security descriptor", e))?;
    let descriptor = Descriptor(raw);
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0 .0,
        bInheritHandle: false.into(),
    };
    Ok((descriptor, attrs))
}

/// 创建受保护目录（已存在则不改变其 ACL，由 [`verify_private`] 单独校验）。
pub fn create_private_dir(path: &std::path::Path) -> Result<(), DshError> {
    let sid = current_sid()?;
    let sddl = protected_sddl(&sid, true);
    let (_descriptor, mut attrs) = security_attributes(&sddl)?;
    let path = wide(path)?;
    let result = unsafe {
        windows::Win32::Storage::FileSystem::CreateDirectoryW(
            PCWSTR(path.as_ptr()),
            Some(&mut attrs as *mut SECURITY_ATTRIBUTES),
        )
    };
    match result {
        // 已存在即视为成功：后续 ACL 校验负责判定它是否可信。
        Ok(()) => Ok(()),
        Err(e) if e.code() == windows::Win32::Foundation::ERROR_ALREADY_EXISTS.to_hresult() => {
            Ok(())
        }
        Err(e) => Err(state_error("create private directory", e)),
    }
}

/// 以受保护 DACL 创建新文件并写入字节（CREATE_NEW，避免跟随预置对象）。
pub fn create_private_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), DshError> {
    use std::io::Write;
    let sid = current_sid()?;
    let sddl = protected_sddl(&sid, false);
    let (_descriptor, mut attrs) = security_attributes(&sddl)?;
    let path = wide(path)?;
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
            Some(&mut attrs as *mut SECURITY_ATTRIBUTES),
            windows::Win32::Storage::FileSystem::CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| state_error("create private file", e))?;
    let mut file = unsafe { std::fs::File::from_raw_handle(handle.0) };
    file.write_all(bytes)
        .map_err(|e| state_error("write private file", e))?;
    file.sync_all()
        .map_err(|e| state_error("flush private file", e))?;
    Ok(())
}

/// 逐 ACE 校验现有对象：owner 为当前用户、DACL 受保护，且仅当前用户 / SYSTEM /
/// Administrators 拥有访问权；继承来的授权一律拒绝。
///
/// 校验失败返回错误——调用方不得把失败当成「文件不存在」而重建密钥或空账本。
pub fn verify_private(path: &std::path::Path) -> Result<(), DshError> {
    use windows::Win32::Security::{
        GetAce, ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSID, SE_DACL_PROTECTED,
    };
    let sid = current_sid()?;
    let is_dir = std::fs::symlink_metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    let path = wide(path)?;
    // 目录必须带 BACKUP_SEMANTICS；文件使用普通属性打开。
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            windows::Win32::Storage::FileSystem::OPEN_EXISTING,
            if is_dir {
                windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            None,
        )
    }
    .map_err(|e| state_error("open for acl verification", e))?;
    let handle = OwnedHandle(handle);
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let result = unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
    };
    if result != ERROR_SUCCESS {
        return Err(state_error(
            "read file security info",
            format!("{result:?}"),
        ));
    }
    let _full = Descriptor(descriptor);
    if dacl.is_null() {
        return Err(DshError::new(
            codes::IPC_STATE,
            "private file has no DACL (world-accessible)",
        ));
    }
    // 受保护位：拒绝继承来的授权（宽父目录不能悄悄授权其他主体）。
    let mut control = 0u16;
    let mut revision = 0u32;
    unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) }
        .map_err(|e| state_error("read security descriptor control", e))?;
    if control & SE_DACL_PROTECTED.0 == 0 {
        return Err(DshError::new(
            codes::IPC_STATE,
            "private file DACL is not protected (inherited access possible)",
        ));
    }
    let owner_text = sid_string(owner)?;
    if owner_text != sid {
        return Err(DshError::new(
            codes::IPC_STATE,
            "private file owner is not the current user",
        ));
    }
    // 逐 ACE：只接受 ACCESS_ALLOWED_ACE_TYPE；其他允许型（callback 等）一律拒绝，
    // 不能因「不认识」而跳过——跳过就等于放过未核验的授权主体。
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const INHERITED_ACE: u8 = 0x10;
    const OBJECT_INHERIT_ACE: u8 = 0x01;
    const CONTAINER_INHERIT_ACE: u8 = 0x02;
    let ace_count = unsafe { (*dacl).AceCount };
    for index in 0..ace_count {
        let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
        unsafe { GetAce(dacl, index as u32, &mut ace) }
            .map_err(|e| state_error("enumerate private file ACE", e))?;
        let header = unsafe { &*(ace as *const windows::Win32::Security::ACE_HEADER) };
        match header.AceType {
            ACCESS_ALLOWED_ACE_TYPE => {}
            ACCESS_DENIED_ACE_TYPE => {
                return Err(DshError::new(
                    codes::IPC_STATE,
                    "private file DACL contains a deny ACE",
                ))
            }
            other => {
                return Err(DshError::new(
                    codes::IPC_STATE,
                    format!("private file DACL contains unsupported ACE type {other}"),
                ))
            }
        }
        if header.AceFlags & INHERITED_ACE != 0 {
            return Err(DshError::new(
                codes::IPC_STATE,
                "private file DACL contains an inherited ACE",
            ));
        }
        // 目录允许 OI/CI 向下传播（创建时就是这样写的）；文件不得可继承。
        let allowed_flags = if is_dir {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        if header.AceFlags & !INHERITED_ACE & !allowed_flags != 0 {
            return Err(DshError::new(
                codes::IPC_STATE,
                "private file DACL contains unsupported ACE flags",
            ));
        }
        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        let ace_sid = PSID(&allowed.SidStart as *const u32 as *mut std::ffi::c_void);
        let text = sid_string(ace_sid)?;
        if !trusted_principal(&text, &sid) {
            return Err(DshError::new(
                codes::IPC_STATE,
                format!("private file grants access to unexpected principal: {text}"),
            ));
        }
    }
    Ok(())
}

fn sid_string(sid: windows::Win32::Security::PSID) -> Result<String, DshError> {
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut text) }
        .map_err(|e| state_error("convert ACE SID", e))?;
    let result = unsafe { text.to_string() }.map_err(|e| state_error("decode ACE SID", e));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    result
}

/// 当前用户、LOCAL SYSTEM、内置 Administrators 之外的任何主体都不接受。
fn trusted_principal(text: &str, current: &str) -> bool {
    text == current || text == "S-1-5-18" || text == "S-1-5-32-544"
}

/// 校验路径（文件或目录）在父目录下不存在有效 reparse point。
pub fn reject_reparse(path: &std::path::Path) -> Result<(), DshError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| state_error("stat private path", e))?;
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(DshError::new(
            codes::IPC_STATE,
            "private path is a reparse point",
        ));
    }
    Ok(())
}

pub fn is_dir(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

//! ZCode 账号凭据的系统保险箱适配口（设计 §3）。
//!
//! 本模块只负责**一个账号、一个条目**的持久化，不保存 terminal token、OAuth
//! code/state 或第三方 access_token。含 JWT 的 [`StoredCredential`] 不实现
//! `Debug`，不作为 IPC DTO；错误集合封闭，不携带原生错误文本或 bytes。
//!
//! 单元测试只覆盖纯函数与记录型 fake；真实 Keychain／Credential Manager 的
//! 平台验收在实现之后的平台上单独进行（设计 §7）。

use crate::database::dao::settings::{try_get_setting_conn, try_set_setting_conn};
use crate::remote::pairing::DeviceStore;
use crate::services::zcode_control::auth::OAuthProvider;
use rusqlite::Connection;
#[cfg(test)]
use std::sync::Arc;

/// 记录格式版本（当前唯一允许值）。
pub(crate) const SCHEMA_VERSION: u8 = 1;

/// 单个条目 secret 的字节上限（Windows 通用凭据的 blob 上限）。
pub(crate) const MAX_CREDENTIAL_BYTES: usize = 2560;

/// 正式版「允许自动恢复」非敏感标记（设计 §3.3）。
///
/// 恢复标记与保险箱条目同命名空间：正式版 `com.jarvis.tuvis.zcode` ↔
/// `remote.zcode_restore_allowed`；开发版 `com.jarvis.tuvis.dev.zcode` ↔
/// `remote.zcode_restore_allowed.dev`（见 [`restore_allowed_key_for`]）。
pub(crate) const RESTORE_ALLOWED_KEY_RELEASE: &str = "remote.zcode_restore_allowed";

/// 开发版「允许自动恢复」非敏感标记（设计 §3.3）。
pub(crate) const RESTORE_ALLOWED_KEY_DEBUG: &str = "remote.zcode_restore_allowed.dev";

/// 纯函数：按构建选择恢复标记键。
///
/// 两个构建共用同一个 settings 库（`~/.tuvis/tuvis.db`），若共用标记键，开发版的一次
/// 忘记/删除会把正式版的恢复禁用掉（保险箱条目本身已按构建隔离）。旧共享键不迁移：
/// 本功能尚未发布，开发版不再读写非 `.dev` 键。
pub(crate) const fn restore_allowed_key_for(debug: bool) -> &'static str {
    if debug {
        RESTORE_ALLOWED_KEY_DEBUG
    } else {
        RESTORE_ALLOWED_KEY_RELEASE
    }
}

/// 系统保险箱条目的固定 user／target 后缀（设计 §3.1）。
const ACCOUNT: &str = "primary";

/// 凭据存储的封闭错误集合：无原始 cause，也不含 bytes/JWT。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialError {
    LockedOrDenied,
    Unavailable,
    StorageFailed,
    InvalidRecord,
    TooLarge,
    DeleteFailed,
}

pub(crate) type StoreResult<T> = Result<T, CredentialError>;

/// 一个账号的 ZCode 凭据（OAuth 取得的账号 JWT 与 provider）。
///
/// 刻意不实现 `Debug`：值不得进入日志、IPC 或错误信息。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StoredCredential {
    pub(crate) schema_version: u8,
    pub(crate) provider: OAuthProvider,
    pub(crate) jwt: String,
}

/// 系统凭据存储适配口。同步原生调用由 AuthService 在锁外的单飞 blocking 任务执行。
pub(crate) trait CredentialStore: Send + Sync {
    fn load(&self) -> StoreResult<Option<StoredCredential>>;
    fn save(&self, record: &StoredCredential) -> StoreResult<()>;
    fn delete(&self) -> StoreResult<()>;
}

/// secret 的线上格式（camelCase JSON bytes；只在 Rust 内序列化／解析）。
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordWire {
    schema_version: u8,
    provider: String,
    jwt: String,
}

fn validate_record(record: &StoredCredential) -> StoreResult<()> {
    if record.schema_version != SCHEMA_VERSION || record.jwt.trim().is_empty() {
        return Err(CredentialError::InvalidRecord);
    }
    Ok(())
}

/// 把记录编码为 UTF-8 JSON bytes（不是 UTF-16 password API；不额外加密）。
pub(crate) fn encode_record(record: &StoredCredential) -> StoreResult<Vec<u8>> {
    validate_record(record)?;
    let wire = RecordWire {
        schema_version: record.schema_version,
        provider: record.provider.literal().to_string(),
        jwt: record.jwt.clone(),
    };
    let bytes = serde_json::to_vec(&wire).map_err(|_| CredentialError::InvalidRecord)?;
    validate_blob_len(bytes.len(), Some(MAX_CREDENTIAL_BYTES))?;
    Ok(bytes)
}

/// 解析 secret bytes：畸形/未知 schema/未知 provider/空 JWT/超限一律拒绝。
pub(crate) fn decode_record(bytes: &[u8]) -> StoreResult<StoredCredential> {
    validate_blob_len(bytes.len(), Some(MAX_CREDENTIAL_BYTES))?;
    let wire: RecordWire =
        serde_json::from_slice(bytes).map_err(|_| CredentialError::InvalidRecord)?;
    if wire.schema_version != SCHEMA_VERSION || wire.jwt.trim().is_empty() {
        return Err(CredentialError::InvalidRecord);
    }
    let provider = match wire.provider.as_str() {
        "bigmodel" => OAuthProvider::Bigmodel,
        "zai" => OAuthProvider::Zai,
        _ => return Err(CredentialError::InvalidRecord),
    };
    Ok(StoredCredential {
        schema_version: wire.schema_version,
        provider,
        jwt: wire.jwt,
    })
}

/// 平台 blob 长度校验（`None` = 平台无额外上限）。
pub(crate) fn validate_blob_len(bytes: usize, max: Option<usize>) -> StoreResult<()> {
    match max {
        Some(max) if bytes > max => Err(CredentialError::TooLarge),
        _ => Ok(()),
    }
}

/// 恢复禁用标记：checked read——表缺失/DB 错误是失败，不是「缺键」。
fn read_restore_allowed(conn: &Connection) -> rusqlite::Result<bool> {
    read_restore_allowed_for(conn, cfg!(debug_assertions))
}

/// 按构建（`debug`）读取恢复标记；测试用它证明两个命名空间互不读取。
fn read_restore_allowed_for(conn: &Connection, debug: bool) -> rusqlite::Result<bool> {
    match try_get_setting_conn(conn, restore_allowed_key_for(debug))? {
        None => Ok(true),
        // 设计 §3.3：非法值按 false 处理（不恢复即安全侧）。
        Some(value) => Ok(value == "true"),
    }
}

fn write_restore_allowed(conn: &Connection, allowed: bool) -> rusqlite::Result<()> {
    write_restore_allowed_for(conn, allowed, cfg!(debug_assertions))
}

/// 按构建（`debug`）写入恢复标记（键见 [`restore_allowed_key_for`]）。
fn write_restore_allowed_for(
    conn: &Connection,
    allowed: bool,
    debug: bool,
) -> rusqlite::Result<()> {
    try_set_setting_conn(
        conn,
        restore_allowed_key_for(debug),
        if allowed { "true" } else { "false" },
    )
}

/// 纯编排：禁用恢复时绝不触碰原生读取；允许时读取并解析记录。
fn load_record<F>(allowed: bool, read: F) -> StoreResult<Option<StoredCredential>>
where
    F: FnOnce() -> StoreResult<Option<Vec<u8>>>,
{
    if !allowed {
        return Ok(None);
    }
    let Some(bytes) = read()? else {
        return Ok(None);
    };
    decode_record(&bytes).map(Some)
}

/// 纯编排：先禁用恢复、再写原生存储、成功后才重新允许恢复。
///
/// 原生写失败时立即返回（恢复标记保持禁用）；enable 失败统一报
/// `StorageFailed`——非敏感标记失败不得宣称「已记住」。
fn save_record<D, W, E>(bytes: &[u8], disable: D, write: W, enable: E) -> StoreResult<()>
where
    D: FnOnce() -> StoreResult<()>,
    W: FnOnce(&[u8]) -> StoreResult<()>,
    E: FnOnce() -> StoreResult<()>,
{
    disable()?;
    write(bytes)?;
    enable().map_err(|_| CredentialError::StorageFailed)
}

/// 纯编排：先禁用恢复再删除；删除失败（NoEntry 已由平台层归一为成功）如实回报。
fn delete_record<D, X>(disable: D, delete: X) -> StoreResult<()>
where
    D: FnOnce() -> StoreResult<()>,
    X: FnOnce() -> StoreResult<()>,
{
    disable()?;
    delete()
}

/// 生产实现：用户级系统保险箱 + settings 中的非敏感恢复标记。
///
/// C3 起由 [`super::hub::PartsBackend::production`] 显式注入 `AuthService`；构造只保存
/// `DeviceStore`，首次真实 OS 访问发生在 `restore`/`save`/`delete`。
pub(crate) struct OsCredentialStore {
    prefs: DeviceStore,
}

impl OsCredentialStore {
    pub(crate) fn new(prefs: DeviceStore) -> Self {
        Self { prefs }
    }

    fn restore_allowed(&self) -> StoreResult<bool> {
        self.prefs
            .with(read_restore_allowed)
            .map_err(|_| CredentialError::StorageFailed)
    }

    fn set_restore_allowed(&self, allowed: bool) -> StoreResult<()> {
        self.prefs
            .with(|conn| write_restore_allowed(conn, allowed))
            .map_err(|_| CredentialError::StorageFailed)
    }
}

impl CredentialStore for OsCredentialStore {
    fn load(&self) -> StoreResult<Option<StoredCredential>> {
        let allowed = self.restore_allowed()?;
        load_record(allowed, native::read_secret)
    }

    fn save(&self, record: &StoredCredential) -> StoreResult<()> {
        // 先做纯格式校验：非法记录不得先把恢复禁用掉。
        let bytes = encode_record(record)?;
        save_record(
            &bytes,
            || self.set_restore_allowed(false),
            native::write_secret,
            || self.set_restore_allowed(true),
        )
    }

    fn delete(&self) -> StoreResult<()> {
        // 先禁用恢复：删除失败也不会在重启后恢复旧条目；成功后保持禁用。
        delete_record(|| self.set_restore_allowed(false), native::delete_secret)
    }
}

/// macOS 用户 Keychain／Linux Secret Service：keyring 3.6.3 的对应原生后端。
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native {
    use super::{validate_blob_len, CredentialError, StoreResult, ACCOUNT, MAX_CREDENTIAL_BYTES};

    const SERVICE: &str = "com.jarvis.tuvis.zcode";
    const SERVICE_DEBUG: &str = "com.jarvis.tuvis.dev.zcode";

    fn service_name() -> &'static str {
        if cfg!(debug_assertions) {
            SERVICE_DEBUG
        } else {
            SERVICE
        }
    }

    fn entry() -> StoreResult<keyring::Entry> {
        keyring::Entry::new(service_name(), ACCOUNT).map_err(map_keyring_error)
    }

    /// keyring 错误 → 封闭枚举：丢弃原始错误（含 `BadEncoding` 的 bytes）。
    fn map_keyring_error(error: keyring::Error) -> CredentialError {
        match error {
            keyring::Error::NoStorageAccess(_) => CredentialError::LockedOrDenied,
            keyring::Error::TooLong(..) => CredentialError::TooLarge,
            keyring::Error::BadEncoding(_)
            | keyring::Error::Ambiguous(_)
            | keyring::Error::Invalid(..) => CredentialError::InvalidRecord,
            // NoEntry 由调用点处理；PlatformFailure／未知一律 unavailable。
            _ => CredentialError::Unavailable,
        }
    }

    /// 纯映射（供单测覆盖 NoEntry/锁定/坏编码，不访问真实保险箱）。
    pub(super) fn classify_keyring_load(
        result: Result<Vec<u8>, keyring::Error>,
    ) -> StoreResult<Option<Vec<u8>>> {
        match result {
            Ok(bytes) => Ok(Some(bytes)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    pub(super) fn read_secret() -> StoreResult<Option<Vec<u8>>> {
        classify_keyring_load(entry()?.get_secret())
    }

    pub(super) fn write_secret(bytes: &[u8]) -> StoreResult<()> {
        validate_blob_len(bytes.len(), Some(MAX_CREDENTIAL_BYTES))?;
        entry()?.set_secret(bytes).map_err(map_keyring_error)
    }

    /// 纯映射（供单测覆盖）：NoEntry 视为已删除；存储不可访问仍报锁定/拒绝；
    /// 其余删除错误统一 `DeleteFailed`（不泄漏原生错误）。
    pub(super) fn classify_keyring_delete(result: Result<(), keyring::Error>) -> StoreResult<()> {
        match result {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(keyring::Error::NoStorageAccess(_)) => Err(CredentialError::LockedOrDenied),
            Err(_) => Err(CredentialError::DeleteFailed),
        }
    }

    pub(super) fn delete_secret() -> StoreResult<()> {
        let entry = entry().map_err(|error| match error {
            CredentialError::LockedOrDenied => CredentialError::LockedOrDenied,
            _ => CredentialError::DeleteFailed,
        })?;
        classify_keyring_delete(entry.delete_credential())
    }
}

/// Windows：复用 `windows` 0.62 的 Credential API（不用 keyring 的 windows-native，
/// 后者默认 `CRED_PERSIST_ENTERPRISE`，与本设计的「当前用户、同机」范围不符）。
#[cfg(target_os = "windows")]
mod native {
    use super::{validate_blob_len, CredentialError, StoreResult, ACCOUNT, MAX_CREDENTIAL_BYTES};
    use windows::core::{Error as WinError, HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NOT_FOUND, FILETIME};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    const SERVICE: &str = "com.jarvis.tuvis.zcode";
    const SERVICE_DEBUG: &str = "com.jarvis.tuvis.dev.zcode";

    fn service_name() -> &'static str {
        if cfg!(debug_assertions) {
            SERVICE_DEBUG
        } else {
            SERVICE
        }
    }

    /// Windows 以 target name 作为唯一标识；固定 `service.primary` 后缀。
    fn target_name() -> String {
        format!("{}.{}", service_name(), ACCOUNT)
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn is_not_found(error: &WinError) -> bool {
        error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0)
    }

    fn is_access_denied(error: &WinError) -> bool {
        error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
    }

    /// 纯映射：条目不存在视为已删除；拒绝访问报锁定/拒绝；其余统一删除失败。
    fn classify_windows_delete(result: windows::core::Result<()>) -> StoreResult<()> {
        match result {
            Ok(()) => Ok(()),
            Err(error) if is_not_found(&error) => Ok(()),
            Err(error) if is_access_denied(&error) => Err(CredentialError::LockedOrDenied),
            Err(_) => Err(CredentialError::DeleteFailed),
        }
    }

    /// `CredReadW` 返回的缓冲区必须由 `CredFree` 释放；RAII 覆盖全部早退路径。
    struct CredentialBuffer(*mut CREDENTIALW);

    impl Drop for CredentialBuffer {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CredFree(self.0 as *const core::ffi::c_void) }
            }
        }
    }

    pub(super) fn read_secret() -> StoreResult<Option<Vec<u8>>> {
        let target = wide(&target_name());
        let mut raw: *mut CREDENTIALW = std::ptr::null_mut();
        match unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut raw) } {
            Ok(()) => {
                let buffer = CredentialBuffer(raw);
                let credential = unsafe { &*buffer.0 };
                let len = credential.CredentialBlobSize as usize;
                validate_blob_len(len, Some(MAX_CREDENTIAL_BYTES))?;
                if len == 0 || credential.CredentialBlob.is_null() {
                    return Err(CredentialError::InvalidRecord);
                }
                let bytes =
                    unsafe { std::slice::from_raw_parts(credential.CredentialBlob, len).to_vec() };
                Ok(Some(bytes))
            }
            Err(error) if is_not_found(&error) => Ok(None),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }

    pub(super) fn write_secret(bytes: &[u8]) -> StoreResult<()> {
        validate_blob_len(bytes.len(), Some(MAX_CREDENTIAL_BYTES))?;
        let mut target = wide(&target_name());
        let mut blob = bytes.to_vec();
        let credential = CREDENTIALW {
            Flags: CRED_FLAGS(0),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            Comment: PWSTR::null(),
            LastWritten: FILETIME::default(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: PWSTR::null(),
            UserName: PWSTR::null(),
        };
        match unsafe { CredWriteW(&credential, 0) } {
            Ok(()) => Ok(()),
            Err(_) => Err(CredentialError::StorageFailed),
        }
    }

    pub(super) fn delete_secret() -> StoreResult<()> {
        let target = wide(&target_name());
        classify_windows_delete(unsafe {
            CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None)
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 纯映射：不存在=幂等成功，拒绝访问=锁定/拒绝，其余=删除失败
        #[test]
        fn windows_delete_error_codes_map_per_design() {
            let not_found = WinError::from_hresult(HRESULT::from_win32(ERROR_NOT_FOUND.0));
            assert!(classify_windows_delete(Err(not_found)).is_ok());

            let denied = WinError::from_hresult(HRESULT::from_win32(ERROR_ACCESS_DENIED.0));
            assert_eq!(
                classify_windows_delete(Err(denied)).unwrap_err(),
                CredentialError::LockedOrDenied
            );

            let other = WinError::from_hresult(HRESULT::from_win32(0x1234));
            assert_eq!(
                classify_windows_delete(Err(other)).unwrap_err(),
                CredentialError::DeleteFailed
            );
        }
    }
}

/// 其他平台（含移动端）：没有实现系统保险箱，如实报不可用，不落文件/内存假存储。
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod native {
    use super::{CredentialError, StoreResult};

    pub(super) fn read_secret() -> StoreResult<Option<Vec<u8>>> {
        Err(CredentialError::Unavailable)
    }

    pub(super) fn write_secret(_bytes: &[u8]) -> StoreResult<()> {
        Err(CredentialError::Unavailable)
    }

    pub(super) fn delete_secret() -> StoreResult<()> {
        Err(CredentialError::DeleteFailed)
    }
}

/// 记录型内存 fake（仅测试）：授权保存／恢复／故障注入都经它，
/// 绝不接触真实系统保险箱。
///
/// H2 起另带**精确闸门**与调用探针：`gate_load/gate_save/gate_delete` 把对应操作停在
/// 「已进入 fake、尚未完成」的窗口，供测试构造确定交错（不靠真实 sleep）；
/// 探针记录同时进入 fake 的最大调用数，串行缺陷会在这里暴露。
#[cfg(test)]
pub(crate) struct MemoryCredentialStore {
    state: std::sync::Mutex<MemoryState>,
    /// 每类操作最多一个待用闸门，操作进入时取走（一次性）。
    gates: std::sync::Mutex<StoreGates>,
    /// 并发探针：同时进入 fake 的调用数上限。
    calls: StoreCallProbe,
}

#[cfg(test)]
struct MemoryState {
    record: Option<StoredCredential>,
    failure: Option<CredentialError>,
    loads: usize,
    saves: usize,
    deletes: usize,
}

/// 一次被闸门挂起的原生调用（fake 侧）。
#[cfg(test)]
struct GateSlot {
    /// 操作进入后通知测试（`Notify` 会存一个许可，先到先得）。
    entered: Arc<tokio::sync::Notify>,
    /// 测试放行；发送端被丢弃同样视为放行（测试失败也不会悬挂 blocking 线程）。
    release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
#[derive(Default)]
struct StoreGates {
    load: Option<GateSlot>,
    save: Option<GateSlot>,
    delete: Option<GateSlot>,
}

/// 闸门句柄（测试侧）：`wait_entered()` 等到操作真正进入 fake，`release()` 放行。
#[cfg(test)]
pub(crate) struct StoreGate {
    entered: Arc<tokio::sync::Notify>,
    release: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(test)]
impl StoreGate {
    /// 等到对应操作进入 fake（此时它正阻塞在放行之前）。
    ///
    /// 带超时：闸门若因实现缺陷从未被触发，测试必须以明确失败收场而不是永久悬挂。
    pub(crate) async fn wait_entered(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), self.entered.notified())
            .await
            .expect("等待存储操作进入 fake 超时（闸门未被触发）");
    }

    /// 放行一次被挂起的操作；drop 未放行的闸门等同放行。
    pub(crate) fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum StoreOp {
    Load,
    Save,
    Delete,
}

/// 并发探针：进入时递增活动调用数并刷新最大值，guard 析构时递减。
#[cfg(test)]
#[derive(Default)]
struct StoreCallProbe {
    active: std::sync::atomic::AtomicUsize,
    max: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
struct StoreCallGuard<'a> {
    probe: &'a StoreCallProbe,
}

#[cfg(test)]
impl Drop for StoreCallGuard<'_> {
    fn drop(&mut self) {
        self.probe
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl MemoryCredentialStore {
    pub(crate) fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(MemoryState {
                record: None,
                failure: None,
                loads: 0,
                saves: 0,
                deletes: 0,
            }),
            gates: std::sync::Mutex::new(StoreGates::default()),
            calls: StoreCallProbe::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn seed(&self, record: StoredCredential) {
        self.lock().record = Some(record);
    }

    pub(crate) fn fail_next(&self, error: CredentialError) {
        self.lock().failure = Some(error);
    }

    /// 当前持久记录快照（C2：断言保存/删除/覆盖后的内容，不算作 load 计数）。
    pub(crate) fn stored(&self) -> Option<StoredCredential> {
        self.lock().record.clone()
    }

    pub(crate) fn load_count(&self) -> usize {
        self.lock().loads
    }

    pub(crate) fn save_count(&self) -> usize {
        self.lock().saves
    }

    pub(crate) fn delete_count(&self) -> usize {
        self.lock().deletes
    }

    /// fake 内观察到的最大同时在途调用数（H2 串行断言：必须恒为 1）。
    pub(crate) fn max_concurrent_store_calls(&self) -> usize {
        self.calls.max.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 让下一次 `load` 停在「已进入、未完成」窗口。
    pub(crate) fn gate_load(&self) -> StoreGate {
        self.install_gate(StoreOp::Load)
    }

    /// 让下一次 `save` 停在「已进入、未完成」窗口。
    pub(crate) fn gate_save(&self) -> StoreGate {
        self.install_gate(StoreOp::Save)
    }

    /// 让下一次 `delete` 停在「已进入、未完成」窗口。
    pub(crate) fn gate_delete(&self) -> StoreGate {
        self.install_gate(StoreOp::Delete)
    }

    fn install_gate(&self, op: StoreOp) -> StoreGate {
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, receiver) = tokio::sync::oneshot::channel();
        let slot = GateSlot {
            entered: entered.clone(),
            release: receiver,
        };
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match op {
            StoreOp::Load => gates.load = Some(slot),
            StoreOp::Save => gates.save = Some(slot),
            StoreOp::Delete => gates.delete = Some(slot),
        }
        StoreGate {
            entered,
            release: Some(release),
        }
    }

    fn enter_call(&self) -> StoreCallGuard<'_> {
        let active = self
            .calls
            .active
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.calls
            .max
            .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
        StoreCallGuard { probe: &self.calls }
    }

    /// 调用计数在**进入 fake** 时递增（与完成与否无关，测试可据此判断操作已开始）。
    fn count_call(&self, op: StoreOp) {
        let mut state = self.lock();
        match op {
            StoreOp::Load => state.loads += 1,
            StoreOp::Save => state.saves += 1,
            StoreOp::Delete => state.deletes += 1,
        }
    }

    /// 进入 fake 后通知测试并等待放行；未安装闸门时是空操作。
    fn wait_gate(&self, op: StoreOp) {
        let slot = {
            let mut gates = self
                .gates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match op {
                StoreOp::Load => gates.load.take(),
                StoreOp::Save => gates.save.take(),
                StoreOp::Delete => gates.delete.take(),
            }
        };
        if let Some(slot) = slot {
            slot.entered.notify_one();
            // 发送端被 drop 也返回 Err：视为放行，避免测试失败时悬挂 blocking 线程。
            let _ = slot.release.blocking_recv();
        }
    }
}

#[cfg(test)]
impl CredentialStore for MemoryCredentialStore {
    fn load(&self) -> StoreResult<Option<StoredCredential>> {
        let _call = self.enter_call();
        self.count_call(StoreOp::Load);
        self.wait_gate(StoreOp::Load);
        let mut state = self.lock();
        if let Some(error) = state.failure.take() {
            return Err(error);
        }
        Ok(state.record.clone())
    }

    fn save(&self, record: &StoredCredential) -> StoreResult<()> {
        let _call = self.enter_call();
        self.count_call(StoreOp::Save);
        self.wait_gate(StoreOp::Save);
        let mut state = self.lock();
        if let Some(error) = state.failure.take() {
            return Err(error);
        }
        state.record = Some(record.clone());
        Ok(())
    }

    fn delete(&self) -> StoreResult<()> {
        let _call = self.enter_call();
        self.count_call(StoreOp::Delete);
        self.wait_gate(StoreOp::Delete);
        let mut state = self.lock();
        if let Some(error) = state.failure.take() {
            return Err(error);
        }
        state.record = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::zcode_control::auth::OAuthProvider;
    use rusqlite::Connection;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    /// 记录型 fixture：常量取自被测实现，避免测试里手抄期望值。
    fn fixture(provider: OAuthProvider) -> StoredCredential {
        StoredCredential {
            schema_version: SCHEMA_VERSION,
            provider,
            jwt: "fixture.jwt.placeholder".to_string(),
        }
    }

    fn pref_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn prefs() -> DeviceStore {
        DeviceStore::memory()
    }

    /// 二进制 UTF-8 记录往返与平台 2,560 字节上限
    #[test]
    fn binary_record_roundtrip_and_limit() {
        let record = fixture(OAuthProvider::Bigmodel);
        let bytes = encode_record(&record).unwrap();
        assert_eq!(
            decode_record(&bytes).unwrap().provider,
            OAuthProvider::Bigmodel
        );
        assert_eq!(validate_blob_len(2560, Some(2560)), Ok(()));
        assert!(validate_blob_len(2561, Some(2560)).is_err());

        // secret 必须是 UTF-8 JSON bytes（不是 UTF-16 password API）：无 NUL 字节
        assert!(std::str::from_utf8(&bytes).is_ok());
        assert!(!bytes.contains(&0));
        let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(wire["schemaVersion"].as_u64() == Some(u64::from(SCHEMA_VERSION)));
        assert!(wire["provider"].as_str() == Some(record.provider.literal()));
        assert!(wire["jwt"].as_str() == Some(record.jwt.as_str()));
    }

    /// 畸形／越界／未知字段记录一律拒绝（错误不携带原始 bytes）
    #[test]
    fn malformed_records_are_rejected() {
        assert!(matches!(
            decode_record(b"not-json"),
            Err(CredentialError::InvalidRecord)
        ));

        let unknown_provider = br#"{"schemaVersion":1,"provider":"other","jwt":"fixture.jwt"}"#;
        assert!(matches!(
            decode_record(unknown_provider),
            Err(CredentialError::InvalidRecord)
        ));

        let unknown_schema_json = br#"{"schemaVersion":2,"provider":"zai","jwt":"fixture.jwt"}"#;
        assert!(matches!(
            decode_record(unknown_schema_json),
            Err(CredentialError::InvalidRecord)
        ));

        let empty_jwt_json = br#"{"schemaVersion":1,"provider":"zai","jwt":""}"#;
        assert!(matches!(
            decode_record(empty_jwt_json),
            Err(CredentialError::InvalidRecord)
        ));

        let unknown_field_json =
            br#"{"schemaVersion":1,"provider":"zai","jwt":"fixture.jwt","extra":true}"#;
        assert!(matches!(
            decode_record(unknown_field_json),
            Err(CredentialError::InvalidRecord)
        ));

        let mut unknown_schema = fixture(OAuthProvider::Zai);
        unknown_schema.schema_version = SCHEMA_VERSION + 1;
        assert_eq!(
            encode_record(&unknown_schema).unwrap_err(),
            CredentialError::InvalidRecord
        );

        let mut empty_jwt = fixture(OAuthProvider::Zai);
        empty_jwt.jwt = "   ".to_string();
        assert_eq!(
            encode_record(&empty_jwt).unwrap_err(),
            CredentialError::InvalidRecord
        );

        let mut huge_jwt = fixture(OAuthProvider::Zai);
        huge_jwt.jwt = "x".repeat(MAX_CREDENTIAL_BYTES);
        assert_eq!(
            encode_record(&huge_jwt).unwrap_err(),
            CredentialError::TooLarge
        );

        assert!(matches!(
            decode_record(&vec![0u8; MAX_CREDENTIAL_BYTES + 1]),
            Err(CredentialError::TooLarge)
        ));
    }

    /// 无条目／重复删除都是幂等成功；计数来自 fake 自身
    #[test]
    fn absence_and_delete_are_idempotent() {
        let fake = MemoryCredentialStore::new();
        assert!(fake.load().unwrap().is_none());
        assert!(fake.delete().is_ok());
        assert!(fake.delete().is_ok());
        assert_eq!(fake.load_count(), 1);
        assert_eq!(fake.delete_count(), 2);
        assert_eq!(fake.save_count(), 0);

        fake.seed(fixture(OAuthProvider::Zai));
        assert!(fake.load().unwrap().is_some());
        assert_eq!(fake.load_count(), 2);
    }

    /// fake 的下一次失败只影响一次调用（供故障注入）
    #[test]
    fn fail_next_is_one_shot() {
        let fake = MemoryCredentialStore::new();
        fake.fail_next(CredentialError::StorageFailed);
        assert_eq!(
            fake.save(&fixture(OAuthProvider::Bigmodel)).unwrap_err(),
            CredentialError::StorageFailed
        );
        assert!(fake.save(&fixture(OAuthProvider::Bigmodel)).is_ok());
        assert_eq!(fake.save_count(), 2);
    }

    /// restore_allowed 缺键=true、非法值=false，读取必须 checked（表缺失=Err）
    #[test]
    fn restore_marker_defaults_true_and_checked_reads() {
        let conn = pref_conn();
        assert!(read_restore_allowed(&conn).unwrap(), "缺键=true");
        write_restore_allowed(&conn, false).unwrap();
        assert!(!read_restore_allowed(&conn).unwrap());
        write_restore_allowed(&conn, true).unwrap();
        assert!(read_restore_allowed(&conn).unwrap());
        // 用当前构建实际选择的键写入非法值（不手抄某一构建的键名）。
        crate::database::dao::settings::try_set_setting_conn(
            &conn,
            restore_allowed_key_for(cfg!(debug_assertions)),
            "bogus",
        )
        .unwrap();
        assert!(!read_restore_allowed(&conn).unwrap(), "非法值=false");

        let broken = Connection::open_in_memory().unwrap();
        assert!(read_restore_allowed(&broken).is_err(), "表缺失必须报错");
    }

    /// P2-2：恢复标记键按构建命名空间隔离（debug / 正式版互不影响）。
    ///
    /// 开发版保险箱条目是 `com.jarvis.tuvis.dev.zcode`，恢复标记也必须落在自己的命名
    /// 空间：共用同一 settings 库时，开发版的禁用标记不得让正式版放弃恢复（反之亦然）。
    #[test]
    fn restore_marker_keys_are_namespaced_per_build() {
        let debug_key = restore_allowed_key_for(true);
        let release_key = restore_allowed_key_for(false);
        assert_ne!(debug_key, release_key, "debug 与正式版必须使用不同标记键");
        assert_eq!(debug_key, "remote.zcode_restore_allowed.dev");
        assert_eq!(release_key, "remote.zcode_restore_allowed");
        // 当前构建按 debug_assertions 选键（测试本身不假定一定跑在 debug 下）。
        let expected_key = if cfg!(debug_assertions) {
            debug_key
        } else {
            release_key
        };
        assert_eq!(
            restore_allowed_key_for(cfg!(debug_assertions)),
            expected_key,
            "生产读写必须按构建选键"
        );

        // 命名空间隔离：正式版/旧共享键的标记对 debug 不可见；debug 的标记也不得
        // 覆盖正式版的禁用状态。
        let conn = pref_conn();
        write_restore_allowed_for(&conn, false, false).unwrap();
        assert!(
            !read_restore_allowed_for(&conn, false).unwrap(),
            "正式版读到自己的禁用标记"
        );
        assert!(
            read_restore_allowed_for(&conn, true).unwrap(),
            "debug 不得读取正式版（旧共享键）的标记"
        );
        write_restore_allowed_for(&conn, false, true).unwrap();
        assert!(!read_restore_allowed_for(&conn, true).unwrap());
        assert!(
            !read_restore_allowed_for(&conn, false).unwrap(),
            "debug 的禁用标记不得改写正式版标记"
        );
        write_restore_allowed_for(&conn, true, true).unwrap();
        assert!(read_restore_allowed_for(&conn, true).unwrap());
        assert!(
            !read_restore_allowed_for(&conn, false).unwrap(),
            "debug 的允许标记不得解除正式版的禁用"
        );
        // 两个键都在 settings 里各存一份，互不覆盖。
        assert_eq!(
            try_get_setting_conn(&conn, debug_key).unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(
            try_get_setting_conn(&conn, release_key).unwrap().as_deref(),
            Some("false")
        );
    }

    /// 禁用恢复时 load 绝不触碰原生读取（read 闭包执行即 panic），只返回 None
    #[test]
    fn load_record_skips_native_read_when_restore_disabled() {
        let result = load_record(false, || {
            panic!("restore disabled: native read must not run")
        });
        assert!(result.unwrap().is_none());
    }

    /// 允许恢复时读取并解析记录；无条目=None，原生错误/坏记录原样回报
    #[test]
    fn load_record_reads_and_decodes_when_allowed() {
        let bytes = encode_record(&fixture(OAuthProvider::Bigmodel)).unwrap();
        let record = load_record(true, || Ok(Some(bytes))).unwrap().unwrap();
        assert!(record.provider == OAuthProvider::Bigmodel);
        assert!(load_record(true, || Ok(None)).unwrap().is_none());
        assert!(matches!(
            load_record(true, || Err(CredentialError::LockedOrDenied)),
            Err(CredentialError::LockedOrDenied)
        ));
        assert!(matches!(
            load_record(true, || Ok(Some(b"not-json".to_vec()))),
            Err(CredentialError::InvalidRecord)
        ));
    }

    /// save 纯编排顺序：disable → native write → enable
    #[test]
    fn save_record_orders_disable_write_enable() {
        let log = RefCell::new(Vec::new());
        let bytes = encode_record(&fixture(OAuthProvider::Bigmodel)).unwrap();
        let result = save_record(
            &bytes,
            || {
                log.borrow_mut().push("disable");
                Ok(())
            },
            |seen| {
                assert!(seen == bytes.as_slice(), "写入的 bytes 必须原样传给原生层");
                log.borrow_mut().push("write");
                Ok(())
            },
            || {
                log.borrow_mut().push("enable");
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert_eq!(*log.borrow(), vec!["disable", "write", "enable"]);
    }

    /// 原生写失败：不得 enable，恢复标记保持禁用
    #[test]
    fn save_record_keeps_restore_disabled_when_native_write_fails() {
        let log = RefCell::new(Vec::new());
        let result = save_record(
            b"secret-bytes",
            || {
                log.borrow_mut().push("disable");
                Ok(())
            },
            |_| {
                log.borrow_mut().push("write");
                Err(CredentialError::StorageFailed)
            },
            || {
                log.borrow_mut().push("enable");
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err(), CredentialError::StorageFailed);
        assert_eq!(*log.borrow(), vec!["disable", "write"]);
    }

    /// disable 标记失败：绝不触碰原生写
    #[test]
    fn save_record_stops_before_native_when_disable_fails() {
        let log = RefCell::new(Vec::new());
        let result = save_record(
            b"secret-bytes",
            || {
                log.borrow_mut().push("disable");
                Err(CredentialError::StorageFailed)
            },
            |_| {
                log.borrow_mut().push("write");
                Ok(())
            },
            || {
                log.borrow_mut().push("enable");
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err(), CredentialError::StorageFailed);
        assert_eq!(*log.borrow(), vec!["disable"]);
    }

    /// enable 标记失败统一报 StorageFailed（不宣称已记住）
    #[test]
    fn save_record_reports_storage_failure_when_enable_fails() {
        let log = RefCell::new(Vec::new());
        let result = save_record(
            b"secret-bytes",
            || {
                log.borrow_mut().push("disable");
                Ok(())
            },
            |_| {
                log.borrow_mut().push("write");
                Ok(())
            },
            || {
                log.borrow_mut().push("enable");
                Err(CredentialError::Unavailable)
            },
        );
        assert_eq!(result.unwrap_err(), CredentialError::StorageFailed);
        assert_eq!(*log.borrow(), vec!["disable", "write", "enable"]);
    }

    /// delete 纯编排顺序：disable → delete（NoEntry 已由平台层归一成功）
    #[test]
    fn delete_record_orders_disable_then_delete() {
        let log = RefCell::new(Vec::new());
        let result = delete_record(
            || {
                log.borrow_mut().push("disable");
                Ok(())
            },
            || {
                log.borrow_mut().push("delete");
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert_eq!(*log.borrow(), vec!["disable", "delete"]);
    }

    /// disable 失败不删除；删除失败原样回报
    #[test]
    fn delete_record_failure_paths() {
        let log = RefCell::new(Vec::new());
        let disabled = delete_record(
            || {
                log.borrow_mut().push("disable");
                Err(CredentialError::StorageFailed)
            },
            || {
                log.borrow_mut().push("delete");
                Ok(())
            },
        );
        assert_eq!(disabled.unwrap_err(), CredentialError::StorageFailed);
        assert_eq!(*log.borrow(), vec!["disable"]);

        let log = RefCell::new(Vec::new());
        let failed = delete_record(
            || {
                log.borrow_mut().push("disable");
                Ok(())
            },
            || {
                log.borrow_mut().push("delete");
                Err(CredentialError::DeleteFailed)
            },
        );
        assert_eq!(failed.unwrap_err(), CredentialError::DeleteFailed);
        assert_eq!(*log.borrow(), vec!["disable", "delete"]);
    }

    /// 标记读取失败必须报 StorageFailed，不能伪装成「未记住」
    #[test]
    fn unreadable_restore_marker_fails_closed() {
        let broken = Connection::open_in_memory().unwrap();
        let prefs = DeviceStore::Owned(Arc::new(Mutex::new(broken)));
        let store = OsCredentialStore::new(prefs);
        assert!(matches!(store.load(), Err(CredentialError::StorageFailed)));
    }

    /// 非法记录在触碰恢复标记/原生存储之前被拒绝
    #[test]
    fn invalid_record_is_rejected_before_native_write() {
        let prefs = prefs();
        let store = OsCredentialStore::new(prefs.clone());
        let mut record = fixture(OAuthProvider::Zai);
        record.schema_version = SCHEMA_VERSION + 1;
        assert_eq!(
            store.save(&record).unwrap_err(),
            CredentialError::InvalidRecord
        );
        assert!(
            prefs.with(read_restore_allowed).unwrap(),
            "非法记录不应先禁用恢复"
        );
    }

    /// NoEntry 在读取路径是「无凭据」而不是错误
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn keyring_absent_entry_is_none_not_error() {
        assert!(matches!(
            native::classify_keyring_load(Err(keyring::Error::NoEntry)),
            Ok(None)
        ));
    }

    /// 原生错误映射到封闭枚举；BadEncoding 的 bytes 不进入错误
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn keyring_locked_and_bad_encoding_map_without_leaking() {
        let locked =
            keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("fixture-locked")));
        assert_eq!(
            native::classify_keyring_load(Err(locked)).unwrap_err(),
            CredentialError::LockedOrDenied
        );

        let bad = keyring::Error::BadEncoding(vec![0xde, 0xad, 0xbe, 0xef]);
        assert!(matches!(
            native::classify_keyring_load(Err(bad)),
            Err(CredentialError::InvalidRecord)
        ));
    }

    /// keyring 删除：NoEntry=幂等成功；NoStorageAccess=锁定/拒绝；其余=删除失败
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn keyring_delete_maps_no_entry_and_denial() {
        assert!(native::classify_keyring_delete(Ok(())).is_ok());
        assert!(native::classify_keyring_delete(Err(keyring::Error::NoEntry)).is_ok());

        let denied =
            keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("fixture-locked")));
        assert_eq!(
            native::classify_keyring_delete(Err(denied)).unwrap_err(),
            CredentialError::LockedOrDenied
        );

        let platform =
            keyring::Error::PlatformFailure(Box::new(std::io::Error::other("fixture-platform")));
        assert_eq!(
            native::classify_keyring_delete(Err(platform)).unwrap_err(),
            CredentialError::DeleteFailed
        );

        let invalid = keyring::Error::Invalid("secret".into(), "fixture".into());
        assert_eq!(
            native::classify_keyring_delete(Err(invalid)).unwrap_err(),
            CredentialError::DeleteFailed
        );
    }

    /// keyring 其余读取错误：TooLong/PlatformFailure/Invalid/Ambiguous
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn keyring_load_maps_remaining_errors() {
        let too_long = keyring::Error::TooLong("secret".into(), 100);
        assert_eq!(
            native::classify_keyring_load(Err(too_long)).unwrap_err(),
            CredentialError::TooLarge
        );

        let platform =
            keyring::Error::PlatformFailure(Box::new(std::io::Error::other("fixture-platform")));
        assert_eq!(
            native::classify_keyring_load(Err(platform)).unwrap_err(),
            CredentialError::Unavailable
        );

        let invalid = keyring::Error::Invalid("secret".into(), "fixture".into());
        assert_eq!(
            native::classify_keyring_load(Err(invalid)).unwrap_err(),
            CredentialError::InvalidRecord
        );

        struct DummyCredential;
        impl keyring::credential::CredentialApi for DummyCredential {
            fn set_secret(&self, _secret: &[u8]) -> keyring::Result<()> {
                Ok(())
            }
            fn get_secret(&self) -> keyring::Result<Vec<u8>> {
                Ok(Vec::new())
            }
            fn delete_credential(&self) -> keyring::Result<()> {
                Ok(())
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        let ambiguous =
            keyring::Error::Ambiguous(vec![Box::new(DummyCredential) as Box<keyring::Credential>]);
        assert_eq!(
            native::classify_keyring_load(Err(ambiguous)).unwrap_err(),
            CredentialError::InvalidRecord
        );
    }
}

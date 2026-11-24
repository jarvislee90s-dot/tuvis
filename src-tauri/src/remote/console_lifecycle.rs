//! 官方控制台宿主凭据与撤权。

use std::sync::{Arc, Mutex};

/// 进程级宿主凭据存储（实例 → 该宿主的**两种**凭据）。
///
/// 为什么是两种（P1-2a）：官方宿主签两类互不通用的凭据——
/// * `/connect?interactions=answer` → **API operator** 凭据（payload `version: 2`），
///   只够开 `/api/remote.mux` 这条 Peer 通道；
/// * `GET /?token=<launchToken>` → **浏览器会话** cookie（payload `version: 1`），
///   官方首页的 `authorizeIndex` 只认它。
///
/// 现役控制台 HTTP 与 WebSocket 均使用浏览器凭据。凭据只在服务器内存交付，
/// 与实例、代际和回环地址绑定。
#[derive(Debug, Clone, Default)]
pub struct UpstreamCredentials {
    /// API operator 凭据（`/api/*`）。
    pub api: String,
    /// 浏览器会话 cookie（首页与静态资源）。空串 = 未交付/兑换失败 → 首页通道
    /// 明确不可用（**不**用 API 凭据顶替）。
    pub browser: String,
    /// 交付时的宿主代际（M02）：凭据**只对签发它的那一代**有效。
    ///
    /// 旧实现按实例 ID 存「最新值」、取用时再按实例搜；宿主换代或换端口后，旧
    /// 能力仍会把新代凭据送往旧地址。这里把代际与 authority 一起存，取用时三项
    /// 必须全部匹配。
    pub epoch: u64,
    /// 交付时的 loopback authority（`127.0.0.1:<port>`）。
    pub authority: String,
    gate: Option<Arc<crate::services::dsh_control::transport::PeerGate>>,
}

impl UpstreamCredentials {
    fn valid(&self) -> bool {
        self.gate.as_ref().is_none_or(|gate| !gate.is_revoked())
    }
}

static UPSTREAM_CREDENTIALS: Mutex<Option<std::collections::HashMap<String, UpstreamCredentials>>> =
    Mutex::new(None);

fn credential_map(
) -> std::sync::MutexGuard<'static, Option<std::collections::HashMap<String, UpstreamCredentials>>>
{
    let mut guard = UPSTREAM_CREDENTIALS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if guard.is_none() {
        *guard = Some(std::collections::HashMap::new());
    }
    guard
}

/// 清除整域控制台凭据并终止现役挂载的连接。
/// 仍处于有效绑定的宿主可在远程服务重开后重新交付凭据。
pub fn revoke_console_access() {
    clear_upstream_credentials();
    crate::remote::console_mount::revoke_all();
}

/// 实例撤权只影响该实例的凭据、能力与活连接。
pub fn revoke_console_access_for_instance(instance_id: &str) {
    clear_upstream_credentials_for(instance_id);
    crate::remote::console_mount::revoke_instance(instance_id);
}

/// 把已受信绑定交付的宿主凭据放进服务端存储。
///
/// 只接受本机授权服务里**已核验的绑定**（调用方传入 `AuthorizedPeer` 产出的
/// cookie），不接受任意字符串；cookie 原文不落日志、不进 DTO。
///
/// `browser` 为空 = 该对端没交浏览器会话 cookie（旧版插件/兑换失败）→ 首页通道
/// 明确不可用，**不拿 `api` 顶替**。
pub fn install_upstream_credentials(
    instance_id: &str,
    api: String,
    browser: String,
    epoch: u64,
    authority: &str,
) -> Result<(), String> {
    install_upstream_credentials_inner(instance_id, api, browser, epoch, authority, None)
}

fn install_upstream_credentials_inner(
    instance_id: &str,
    api: String,
    browser: String,
    epoch: u64,
    authority: &str,
    gate: Option<Arc<crate::services::dsh_control::transport::PeerGate>>,
) -> Result<(), String> {
    if api.trim().is_empty() {
        return Err("CONSOLE_GATEWAY_CREDENTIAL: 控制台凭据为空".to_string());
    }
    let mut guard = credential_map();
    if gate.as_ref().is_some_and(|gate| gate.is_revoked()) {
        return Err("CONSOLE_GATEWAY_CREDENTIAL: 绑定已撤权".to_string());
    }
    let map = guard
        .as_mut()
        .ok_or_else(|| "CONSOLE_GATEWAY_CREDENTIAL: 控制台凭据存储不可用".to_string())?;
    map.insert(
        instance_id.to_owned(),
        UpstreamCredentials {
            api,
            browser,
            epoch,
            authority: authority.to_owned(),
            gate,
        },
    );
    Ok(())
}

#[doc(hidden)]
pub fn install_upstream_credentials_with_gate(
    instance_id: &str,
    api: String,
    browser: String,
    epoch: u64,
    authority: &str,
    gate: Arc<crate::services::dsh_control::transport::PeerGate>,
) -> Result<(), String> {
    let sequence = crate::remote::console_mount::production().revocation_sequence();
    install_upstream_credentials_with_gate_at_sequence(
        instance_id,
        api,
        browser,
        epoch,
        authority,
        gate,
        sequence,
    )
}

pub fn install_upstream_credentials_with_gate_at_sequence(
    instance_id: &str,
    api: String,
    browser: String,
    epoch: u64,
    authority: &str,
    gate: Arc<crate::services::dsh_control::transport::PeerGate>,
    sequence: u64,
) -> Result<(), String> {
    install_upstream_credentials_committing(
        instance_id,
        api,
        browser,
        epoch,
        authority,
        gate,
        super::console_mount::CredentialSequence::Legacy(sequence),
        || {},
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_upstream_credentials_committing(
    instance_id: &str,
    api: String,
    browser: String,
    epoch: u64,
    authority: &str,
    gate: Arc<crate::services::dsh_control::transport::PeerGate>,
    sequence: super::console_mount::CredentialSequence,
    commit_native: impl FnOnce(),
) -> Result<(), String> {
    if api.trim().is_empty() {
        return Err("CONSOLE_GATEWAY_CREDENTIAL: 控制台凭据为空".to_string());
    }
    let browser_for_mount = browser.clone();
    let entry = UpstreamCredentials {
        api,
        browser,
        epoch,
        authority: authority.to_owned(),
        gate: Some(Arc::clone(&gate)),
    };
    let mut credentials = credential_map();
    let map = credentials
        .as_mut()
        .ok_or_else(|| "CONSOLE_GATEWAY_CREDENTIAL: 控制台凭据存储不可用".to_string())?;
    crate::remote::console_mount::production().install_browser_credential_committing(
        instance_id,
        epoch,
        authority,
        browser_for_mount,
        gate,
        sequence,
        || {
            map.insert(instance_id.to_owned(), entry);
            commit_native();
            Ok(())
        },
    )
}

#[cfg(feature = "dsh-test-probes")]
type CredentialCommitHook = Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(feature = "dsh-test-probes")]
static BETWEEN_CONSOLE_CREDENTIAL_COMMITS: std::sync::OnceLock<CredentialCommitHook> =
    std::sync::OnceLock::new();

#[cfg(feature = "dsh-test-probes")]
pub fn init_between_console_credential_commits_hook_for_test(hook: CredentialCommitHook) {
    let _ = BETWEEN_CONSOLE_CREDENTIAL_COMMITS.set(hook);
}

#[cfg(feature = "dsh-test-probes")]
pub(crate) fn observe_console_credential_prepare(instance: &str) {
    if let Some(hook) = BETWEEN_CONSOLE_CREDENTIAL_COMMITS.get() {
        hook(instance);
    }
}

#[cfg(feature = "dsh-test-probes")]
pub fn credential_cache_presence_for_test(instance: &str) -> (bool, bool) {
    let api = credential_map()
        .as_ref()
        .is_some_and(|map| map.contains_key(instance));
    (
        crate::remote::console_mount::production().raw_credential_present(instance),
        api,
    )
}

pub(crate) fn upstream_credential_matches(instance: &str, epoch: u64, authority: &str) -> bool {
    credential_map()
        .as_ref()
        .and_then(|map| map.get(instance))
        .is_some_and(|entry| entry.valid() && entry.epoch == epoch && entry.authority == authority)
}

/// 读取已交付的宿主凭据（只给同进程内的转发代码用；不落日志、不进 DTO）。
pub fn upstream_credential(instance_id: &str) -> Option<String> {
    credential_map()
        .as_ref()
        .and_then(|map| map.get(instance_id))
        .filter(|c| c.valid())
        .map(|c| c.api.clone())
}

/// 读取该实例已交付的浏览器会话 cookie（首页/静态资源用）。
///
/// 返回 `None` = 该实例未交付浏览器凭据（或已兑换失败被留空）→ 调用方必须把
/// 首页通道如实判为不可用，**不得**退化成 API 凭据。
pub fn upstream_browser_cookie(instance_id: &str) -> Option<String> {
    credential_map()
        .as_ref()
        .and_then(|map| map.get(instance_id))
        .filter(|c| c.valid())
        .map(|c| c.browser.clone())
        .filter(|browser| !browser.is_empty())
}

/// 按**完整目标**取浏览器凭据：实例 + 代际 + authority 三项全匹配。
///
/// 这是控制台转发唯一允许的取用方式。任一不符即
/// `None`（fail-closed），杜绝「旧能力把新代凭据送往旧地址」。
pub fn upstream_browser_cookie_for(
    instance_id: &str,
    epoch: u64,
    authority: &str,
) -> Option<String> {
    credential_map()
        .as_ref()
        .and_then(|map| map.get(instance_id))
        .filter(|c| c.valid() && c.epoch == epoch && c.authority == authority)
        .map(|c| c.browser.clone())
        .filter(|browser| !browser.is_empty())
}

/// 清空宿主凭据（撤权/关远程）。
pub fn clear_upstream_credentials() {
    if let Some(map) = credential_map().as_mut() {
        map.clear();
    }
}

/// 只清**该实例**的宿主凭据（P1-4：撤权链按实例收敛；其他实例条目不动）。
pub fn clear_upstream_credentials_for(instance_id: &str) {
    if let Some(map) = credential_map().as_mut() {
        map.remove(instance_id);
    }
}

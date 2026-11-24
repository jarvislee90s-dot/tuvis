//! H09 控制台能力路径：绑定设备 + 实例 + 到期时刻的短期路径令牌。
//!
//! 为什么不用 cookie：已实测同主机不同端口**共享 `mam_device` cookie**，
//! 而 opaque sandbox 内的脚本 `Origin` 为 `null`、且同样拿不到主站 cookie。
//! 因此控制台授权**不依赖任何 cookie**，只靠 URL 里的不透明路径令牌 +
//! **浏览器拒绝把该令牌交给主站**（令牌只存在于 sandbox 文档的 URL 与
//! 该前缀下的请求中）。
//!
//! 纪律：
//! * 令牌与路径前缀一一对应，前缀只允许出现在 `/console/<token>/` 之下；
//! * 令牌不落日志、不进 DTO（对外只给一次入口 URL）；
//! * 到期/撤权/换实例 → 立即失效，且活连接被取消；
//! * 令牌**不是**给主站用的凭据：主站管理 API 完全不认识它。

use super::console_upstream::ConsoleUpstream;
use crate::services::dsh_control::error::codes;
use crate::services::dsh_control::error::DshError;

/// **一次性引导**寿命（毫秒）：链接一旦被用来打开控制台，必须在这个窗口内
/// 完成第一次兑换（`redeem_bootstrap_for_device`），且只能兑换一次。
pub const BOOTSTRAP_TTL_MS: i64 = 60_000;

/// **页面会话**寿命（毫秒）：盖章换出来的令牌，覆盖「首页加载 + 若干交互」。
/// 它绑定同一设备/实例/代际，同样受撤权与换代约束。
pub const CAPABILITY_TTL_MS: i64 = 30 * 60 * 1000;

/// 一条能力记录：绑定设备、实例与上游 authority。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleCapability {
    pub token: String,
    pub device_id: String,
    pub instance_id: String,
    pub authority: String,
    pub expires_at_ms: i64,
    /// 签发时的宿主代际（P1-2c）。上游端口换代后可被复用，旧能力若只比
    /// `authority` 就会把**新代**的凭据送到一个已经不属于该实例的端口上；
    /// 代际变了必须让能力失效（用户重开控制台即可）。
    pub epoch: u64,
    /// 这是一枚**尚未盖章的一次性引导**（P1-2e）：只能用于换取页面会话，
    /// 不能直接服务控制台页面（否则「给人的链接」就成了可转交的长期凭据）。
    pub bootstrap: bool,
}

pub(super) fn issue_bootstrap(
    instance_id: &str,
    device_id: &str,
    now_ms: i64,
    resolve: impl FnOnce(&str) -> Result<ConsoleUpstream, DshError>,
) -> Result<ConsoleCapability, DshError> {
    if device_id.trim().is_empty() {
        return Err(DshError::new(
            codes::CONSOLE_DEVICE_REQUIRED,
            "console capability requires an authenticated device",
        ));
    }
    let upstream = resolve(instance_id)?;
    Ok(ConsoleCapability {
        token: super::random_hex_16(),
        device_id: device_id.to_owned(),
        instance_id: upstream.instance_id,
        authority: upstream.authority,
        epoch: upstream.epoch,
        expires_at_ms: now_ms.saturating_add(BOOTSTRAP_TTL_MS),
        bootstrap: true,
    })
}

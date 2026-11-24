//! H09 控制台网关：上游解析与启动契约。
//!
//! 上游地址**只来自已核验实例的当代活体** `LocalHost.authority`（loopback），
//! 不接受调用方提交的任意上游 URL。这是「不能打开任意用户提交的上游地址」的
//! 结构性保证（Spec N06）。

use crate::services::dsh_control::error::{codes, DshError};
use crate::services::dsh_control::instances::InstanceRegistry;

/// 已核验实例的控制台上游。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleUpstream {
    pub instance_id: String,
    /// `127.0.0.1:<port>` 形态（来自 LocalHost.authority，不是用户输入）。
    pub authority: String,
    /// 该宿主的运行代际（P1-2c）：端口会复用，代际不会。控制台能力绑它，
    /// 换代即失效——否则旧能力会把**新代**的凭据送往一个已不属于该实例的端口。
    pub epoch: u64,
}

/// 解析指定实例的可代理上游。
///
/// 只在以下全部成立时返回：已登记、当代活体唯一、home 归属已核验。
/// 未核验/不在场/多义 → 拒绝（不猜、不回落到别的实例）。
pub fn resolve_upstream(
    registry: &InstanceRegistry,
    instance_id: &str,
) -> Result<ConsoleUpstream, DshError> {
    let registered = registry.registered(instance_id)?;
    let host = registry.resolve_selected()?;
    if host.host_id != registered.instance_id {
        return Err(DshError::new(
            codes::INSTANCE_SWITCHED,
            "selected instance changed while resolving console upstream",
        ));
    }
    if host.authority.trim().is_empty() {
        return Err(DshError::new(
            codes::HOST_LISTENER_MISSING,
            "verified host has no loopback listener",
        ));
    }
    Ok(ConsoleUpstream {
        instance_id: registered.instance_id,
        authority: host.authority,
        epoch: host.epoch,
    })
}

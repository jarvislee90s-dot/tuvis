//! 子任务数据根解析：入口只接收稳定实例 ID，不接受调用方给的任意路径。
//!
//! 生产清单/详情都在此收敛：实例登记 + 会话头归属核验通过后，返回该实例实际
//! `DSH_HOME`（`<profile 上两级>`），再由 source 只读扫描。核验失败原样上抛，
//! 不回落全局 `~/.dsh` 或别的实例。

use crate::services::dsh_control::error::{codes, DshError};
use crate::services::dsh_control::instances::InstanceRegistry;

/// 已登记实例的会话只读根；`session_id` 参与归属核验（必须恰好一处命中）。
pub fn readonly_session_home(
    registry: &InstanceRegistry,
    instance_id: &str,
    session_id: &str,
) -> Result<std::path::PathBuf, DshError> {
    if instance_id.trim().is_empty() {
        return Err(DshError::new(
            codes::SESSION_OWNERSHIP_UNPROVEN,
            "instance id is required for subagent reads",
        ));
    }
    registry.resolve_readonly_root(instance_id, session_id)
}

/// 生产列表源：实例 ID 必须给出且归属核验通过；未给出或核验失败 → 空列表
/// （端点契约的空态唯一权威），绝不偷用全局 `~/.dsh` 猜测实例。
pub fn list_for_instance(
    parent_id: &str,
    instance_id: Option<&str>,
) -> Vec<crate::monitor::subagents::SubagentView> {
    let Some(instance_id) = instance_id.filter(|id| !id.trim().is_empty()) else {
        return Vec::new();
    };
    let Ok(registry) = crate::services::dsh_control::production::production_registry() else {
        return Vec::new();
    };
    match readonly_session_home(&registry, instance_id, parent_id) {
        // 证据增强版列表（2026-11-18 销项批）：chip 需要回合终态（idle + endTs）
        // → collect_with_status（对每个存活子任务多读一次有界全文，实体身份缓存）。
        Ok(home) => crate::monitor::subagents::dsh::collect_with_status(&home, parent_id),
        Err(e) => {
            log::warn!("dsh 子任务列表归属核验失败（{instance_id}）: {e}");
            Vec::new()
        }
    }
}

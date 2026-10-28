//! 能力 key 与已知 build 的 shipped 表（spec R2/§6.3；生产计划 T1）。
//!
//! key 是能力解析的**失效单元**：`platform | app_build | protocol_version |
//! schema_fingerprint | probe_epoch` 任一字段变化都产生不同 key，旧 confirmed
//! 随之失配（T2 账本按 key 存取，T4 resolver 按 key 查询）。
//!
//! shipped 表是**精确匹配**的静态知识：只认已知 build `3.16.2.8769`
//! （macos / windows，protocol 3）。`schema_fingerprint` 与 `probe_epoch` 不参与
//! 命中——它们只决定 key 身份（账本失效），不改变 shipped 知识本身；build 升级
//! 后不走本表（未知 build 由 T4 走接口候选）。

use super::state::{Capability, CapabilityState};

/// 已知 build 的 `app_build` 字面值（spec §6.3：当前唯一已知 build）。
const KNOWN_BUILD: &str = "3.16.2.8769";
/// 已知 build 的协议版本（spec §6.3：protocol 3）。
const KNOWN_PROTOCOL_VERSION: u32 = 3;

/// 能力 key（spec R2）：能力解析的失效单元，五元组任一变化即不同 key。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CapabilityKey {
    pub platform: String,
    pub app_build: String,
    pub protocol_version: u32,
    pub schema_fingerprint: String,
    pub probe_epoch: u32,
}

impl CapabilityKey {
    /// 直接构造（字段语义见结构体文档；不做归一化——调用方给什么就是什么，
    /// 归一化属于 T3 采集侧的职责）。
    pub(crate) fn new(
        platform: &str,
        app_build: &str,
        protocol_version: u32,
        schema_fingerprint: &str,
        probe_epoch: u32,
    ) -> Self {
        Self {
            platform: platform.to_owned(),
            app_build: app_build.to_owned(),
            protocol_version,
            schema_fingerprint: schema_fingerprint.to_owned(),
            probe_epoch,
        }
    }

    /// 账本存储键（spec R6 `zcode_capability_evidence.key` 列）：五字段确定性拼接
    /// `platform|app_build|protocol|schema_fingerprint|epoch`。只含上述字段——
    /// 全部是 build 指纹类标识，不含凭据/令牌；任一字段变化即不同 storage key，
    /// 旧账本记录天然失配（失效单元语义，见模块文档）。
    ///
    /// 字段来源均为受控标识（platform 枚举、build 版本串、探针指纹哈希、u32 epoch），
    /// 不做 `|` 转义——与 `codex_ipc_probe.probe_key` 的既有拼接惯例一致。
    pub(crate) fn to_storage_key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.platform,
            self.app_build,
            self.protocol_version,
            self.schema_fingerprint,
            self.probe_epoch
        )
    }
}

/// 查询已知 build 的 shipped 状态表（spec §6.3）。
///
/// 只认 `("macos"|"windows", "3.16.2.8769", 3)`，其余 key 一律 `None`；
/// `schema_fingerprint` / `probe_epoch` 不参与命中（见模块文档）。命中后按能力：
/// - `send` / `model` / `thought_level` / `approval_reply` → `Confirmed`；
/// - `single_select` / `interrupt` → `InterfaceMatched`（ACK only，首次真实
///   使用确认后才升级）；
/// - `multi_select` / `free_text` → `Unsupported`（合同 false）。
pub(crate) fn shipped_state(
    key: &CapabilityKey,
    capability: Capability,
) -> Option<CapabilityState> {
    let known = (key.platform == "macos" || key.platform == "windows")
        && key.app_build == KNOWN_BUILD
        && key.protocol_version == KNOWN_PROTOCOL_VERSION;
    if !known {
        return None;
    }
    Some(match capability {
        Capability::Send
        | Capability::Model
        | Capability::ThoughtLevel
        | Capability::ApprovalReply => CapabilityState::Confirmed,
        Capability::SingleSelect | Capability::Interrupt => CapabilityState::InterfaceMatched,
        Capability::MultiSelect | Capability::FreeText => CapabilityState::Unsupported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// brief Step 1 逐字用例：build 或 probe_epoch 变化即产生不同 key。
    #[test]
    fn key_changes_when_build_or_epoch_changes() {
        let a = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape-a", 1);
        let b = CapabilityKey::new("macos", "3.16.2.9999", 3, "shape-a", 1);
        let c = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape-a", 2);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    /// T2 账本存储键（spec R6）：五字段确定性拼接，**不含凭据**；任一字段变化
    /// 产生不同 storage key（「key 变化不命中旧记录」的验收点）。
    #[test]
    fn storage_key_distinguishes_all_five_fields() {
        let base = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape-a", 1);
        let others = [
            CapabilityKey::new("windows", "3.16.2.8769", 3, "shape-a", 1),
            CapabilityKey::new("macos", "3.16.2.9999", 3, "shape-a", 1),
            CapabilityKey::new("macos", "3.16.2.8769", 4, "shape-a", 1),
            CapabilityKey::new("macos", "3.16.2.8769", 3, "shape-b", 1),
            CapabilityKey::new("macos", "3.16.2.8769", 3, "shape-a", 2),
        ];
        assert_eq!(
            base.to_storage_key(),
            base.to_storage_key(),
            "同 key 必须得到同 storage key"
        );
        for other in others {
            assert_ne!(
                base.to_storage_key(),
                other.to_storage_key(),
                "任一字段变化必须改变 storage key"
            );
        }
        // 拼接序固定：platform|app_build|protocol|schema_fingerprint|epoch
        assert_eq!(base.to_storage_key(), "macos|3.16.2.8769|3|shape-a|1");
    }

    /// brief Step 1 逐字用例：shipped 表只命中精确 build。
    #[test]
    fn shipped_only_matches_exact_build() {
        let known = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape", 1);
        let unknown = CapabilityKey::new("macos", "3.16.2.9999", 3, "shape", 1);
        assert_eq!(
            shipped_state(&known, Capability::Send),
            Some(CapabilityState::Confirmed)
        );
        assert_eq!(shipped_state(&unknown, Capability::Send), None);
    }

    /// 平台与协议同样参与精确匹配：linux 或 protocol ≠ 3 一律不命中。
    #[test]
    fn shipped_requires_known_platform_and_protocol() {
        let linux = CapabilityKey::new("linux", "3.16.2.8769", 3, "shape", 1);
        let wrong_protocol = CapabilityKey::new("macos", "3.16.2.8769", 4, "shape", 1);
        let windows = CapabilityKey::new("windows", "3.16.2.8769", 3, "shape", 1);
        assert_eq!(shipped_state(&linux, Capability::Send), None);
        assert_eq!(shipped_state(&wrong_protocol, Capability::Send), None);
        assert_eq!(
            shipped_state(&windows, Capability::Send),
            Some(CapabilityState::Confirmed)
        );
    }

    /// spec §6.3 全表：八项能力在已知 build 上的固定取值。
    #[test]
    fn shipped_table_covers_all_eight_capabilities() {
        let known = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape", 1);
        let expected = [
            (Capability::Send, CapabilityState::Confirmed),
            (Capability::SingleSelect, CapabilityState::InterfaceMatched),
            (Capability::MultiSelect, CapabilityState::Unsupported),
            (Capability::FreeText, CapabilityState::Unsupported),
            (Capability::Interrupt, CapabilityState::InterfaceMatched),
            (Capability::ApprovalReply, CapabilityState::Confirmed),
            (Capability::Model, CapabilityState::Confirmed),
            (Capability::ThoughtLevel, CapabilityState::Confirmed),
        ];
        for (capability, state) in expected {
            assert_eq!(shipped_state(&known, capability), Some(state));
        }
    }
}

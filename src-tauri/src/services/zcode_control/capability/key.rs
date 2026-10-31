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
//!
//! 队列三项另走**命令条目**判据（R0 §3 命令表）：同一 build 的官方命令表里出现
//! 对应命令名，才给 `InterfaceMatched`（候选档）；`queue_now` 不在候选层——「立即
//! 执行 vs 引导」的语义没有实机证据。

use super::state::{Capability, CapabilityState};

/// 已知 build 的 `app_build` 字面值（spec §6.3：当前唯一已知 build）。
const KNOWN_BUILD: &str = "3.16.2.8769";
/// 已知 build 的协议版本（spec §6.3：protocol 3）。
const KNOWN_PROTOCOL_VERSION: u32 = 3;

/// 队列候选层用到的官方命令名（R0 §3 命令表 `type` 取值，逐字）。
#[cfg(any(test, feature = "zcode-bridge-evidence"))]
const DELETE_QUEUE_ITEM: &str = "deleteQueueItem";
#[cfg(any(test, feature = "zcode-bridge-evidence"))]
const REORDER_QUEUE_ITEM: &str = "reorderQueueItem";
#[cfg(any(test, feature = "zcode-bridge-evidence"))]
const SEND_QUEUED_NOW: &str = "sendQueuedNow";
#[cfg(any(test, feature = "zcode-bridge-evidence"))]
const SET_AUTO_DRAIN: &str = "setAutoDrain";

/// 已知 build 的**官方命令条目**（R0 §3 命令表，APP `3.16.2.8769`，只登记
/// 取证过的命令名 = 信封 `type` 取值，不含字段与语义）。
///
/// 它是队列候选层的唯一判据：命令名不在表内 = 本 build 未取证该命令，不得开闸。
/// 新增命令必须先补 R0 取证，再进本表（不按「官方差不多应该有」猜）。
#[cfg(any(test, feature = "zcode-bridge-evidence"))]
const KNOWN_BUILD_QUEUE_COMMANDS: [&str; 4] = [
    DELETE_QUEUE_ITEM,
    REORDER_QUEUE_ITEM,
    SEND_QUEUED_NOW,
    SET_AUTO_DRAIN,
];

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
/// - `single_select` / `interrupt` / `multi_select` / `free_text` →
///   `InterfaceMatched`（候选，首次真实使用确认后才升级）。三轮范围裁决
///   （2026-10-29）：「未实测」≠「明确不支持」——历史合同未验证多选/自由文本
///   （证据记录保留在 spec §4.1，不改写为实机通过），但本仓解析（pending 卡
///   multiSelect/freeText 形状）与编码（build_user_answer 多选/自由作答）均已
///   支持，故按接口形状开放为候选；无卡形状时接口闸仍关闭，首次真实动作的
///   确认/降级机制照常兜底。
/// - `queue_edit` / `queue_resume` → 只有 `zcode-bridge-evidence` debug 构建可进入**候选层**，
///   且仅当本 build 命令条目（[`build_declares_queue_command`]，R0 §3）齐备；普通构建
///   一律 `None`。静态命令名不等于 W0 实机取证，不可据此生产开闸。
/// - `queue_now` → **`None`**：`sendQueuedNow` 命令名虽在证据表内，但「立即执行」
///   与「引导当前回合」是否等价没有实机证据，候选层不收录它。
///
/// 候选 ≠ 确认：resolver 对队列的 `InterfaceMatched` 只下发
/// `enabled=true + reasonCode="interface_matched"`，运行时仍由新鲜
/// `availability` / `paused` 二次校验（见 `command::execute_queue_write`）。
pub(crate) fn shipped_state(
    key: &CapabilityKey,
    capability: Capability,
) -> Option<CapabilityState> {
    if !is_known_build(key) {
        return None;
    }
    match capability {
        // W0 未确认的队列命令只能在显式 debug evidence/probe 构建参与候选判断。
        // 普通 dev/release 不因静态命令表而开放产品写能力。
        #[cfg(feature = "zcode-bridge-evidence")]
        Capability::QueueEdit => {
            let edit_commands = [DELETE_QUEUE_ITEM, REORDER_QUEUE_ITEM];
            edit_commands
                .iter()
                .all(|command| build_declares_queue_command(key, command))
                .then_some(CapabilityState::InterfaceMatched)
        }
        #[cfg(not(feature = "zcode-bridge-evidence"))]
        Capability::QueueEdit => None,
        // 「立即执行 vs 引导」语义未实机证实：不进候选层，恒 `needs_evidence`。
        Capability::QueueNow => None,
        #[cfg(feature = "zcode-bridge-evidence")]
        Capability::QueueResume => build_declares_queue_command(key, SET_AUTO_DRAIN)
            .then_some(CapabilityState::InterfaceMatched),
        #[cfg(not(feature = "zcode-bridge-evidence"))]
        Capability::QueueResume => None,
        Capability::Send
        | Capability::Model
        | Capability::ThoughtLevel
        | Capability::ApprovalReply => Some(CapabilityState::Confirmed),
        Capability::SingleSelect
        | Capability::Interrupt
        | Capability::MultiSelect
        | Capability::FreeText => Some(CapabilityState::InterfaceMatched),
    }
}

/// 平台 / build / 协议三项精确命中已知 build（`schema_fingerprint` 与
/// `probe_epoch` 是 key 身份，不参与命中）。
fn is_known_build(key: &CapabilityKey) -> bool {
    (key.platform == "macos" || key.platform == "windows")
        && key.app_build == KNOWN_BUILD
        && key.protocol_version == KNOWN_PROTOCOL_VERSION
}

/// 已知 build 是否宣告了给定官方队列命令（R0 §3 命令表）。
///
/// 只对精确命中的已知 build 生效；未知 build / 未取证命令名一律 `false`
/// ——候选层不得凭「官方大概有这个命令」开闸。
#[cfg(any(test, feature = "zcode-bridge-evidence"))]
pub(crate) fn build_declares_queue_command(key: &CapabilityKey, command: &str) -> bool {
    is_known_build(key) && KNOWN_BUILD_QUEUE_COMMANDS.contains(&command)
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
            // 三轮范围裁决：「未实测」按候选开放（接口形状闸仍生效），不再判死。
            (Capability::MultiSelect, CapabilityState::InterfaceMatched),
            (Capability::FreeText, CapabilityState::InterfaceMatched),
            (Capability::Interrupt, CapabilityState::InterfaceMatched),
            (Capability::ApprovalReply, CapabilityState::Confirmed),
            (Capability::Model, CapabilityState::Confirmed),
            (Capability::ThoughtLevel, CapabilityState::Confirmed),
        ];
        for (capability, state) in expected {
            assert_eq!(shipped_state(&known, capability), Some(state));
        }
    }

    /// 队列写的 shipped 条目 = **候选层**（不是确认）：已知 build 的命令条目命中
    /// `deleteQueueItem`+`reorderQueueItem` → `queue_edit` 候选；`setAutoDrain` →
    /// `queue_resume` 候选；`queue_now` **没有条目**（语义未实机证实）。
    #[cfg(feature = "zcode-bridge-evidence")]
    #[test]
    fn shipped_table_queue_entries_are_candidates_only() {
        let known = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape", 1);
        assert_eq!(
            shipped_state(&known, Capability::QueueEdit),
            Some(CapabilityState::InterfaceMatched),
            "命令条目齐备时 queue_edit 只能进候选层"
        );
        assert_eq!(
            shipped_state(&known, Capability::QueueResume),
            Some(CapabilityState::InterfaceMatched),
            "setAutoDrain 取证 → queue_resume 候选"
        );
        assert_eq!(
            shipped_state(&known, Capability::QueueNow),
            None,
            "queue_now 不进候选层（立即执行 vs 引导未实测）"
        );
    }

    #[cfg(not(feature = "zcode-bridge-evidence"))]
    #[test]
    fn queue_write_commands_stay_closed_without_w0_evidence_build() {
        let known = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape", 1);
        assert_eq!(
            shipped_state(&known, Capability::QueueEdit),
            None,
            "静态命令条目未完成 W0 实机取证，不得在普通构建开放 queue_edit"
        );
        assert_eq!(
            shipped_state(&known, Capability::QueueResume),
            None,
            "静态命令条目未完成 W0 实机取证，不得在普通构建开放 queue_resume"
        );
    }

    /// 命令条目判据只对精确 build 生效：未知 build / 非取证命令名一律 false。
    #[test]
    fn queue_command_evidence_requires_known_build_and_listed_command() {
        let known = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape", 1);
        let unknown = CapabilityKey::new("macos", "3.16.2.9999", 3, "shape", 1);
        let wrong_protocol = CapabilityKey::new("macos", "3.16.2.8769", 4, "shape", 1);
        assert!(build_declares_queue_command(&known, "deleteQueueItem"));
        assert!(build_declares_queue_command(&known, "setAutoDrain"));
        assert!(!build_declares_queue_command(&known, "editQueueItem"));
        assert!(!build_declares_queue_command(&unknown, "deleteQueueItem"));
        assert!(!build_declares_queue_command(
            &wrong_protocol,
            "deleteQueueItem"
        ));
        for capability in [
            Capability::QueueEdit,
            Capability::QueueNow,
            Capability::QueueResume,
        ] {
            assert_eq!(
                shipped_state(&unknown, capability),
                None,
                "{capability:?} 不得在未知 build 上继承命令条目"
            );
        }
    }
}

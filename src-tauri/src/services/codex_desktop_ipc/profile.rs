//! 平台 profile 与按方法版本门（探测定案：版本不匹配表现为 `no-client-found`，
//! 不能判定 owner 不存在；`thread-follower-*` 携带 envelope hostId 时按基值 +1）。
//!
//! profile 常量是**当前协议基线**：`false` = 该 build 尚未活体验证，不是协议不存在；
//! 只有 Task 10 真机闭环 PASS 才翻转对应 flag，测试可用注入 profile 测构造器。
//!
//! **#172 起语义变更（措辞变了，不变量不变）**：`profile_for` 从「准入白名单」降级为
//! 「**证据缓存**」——版本号只做归因与缓存键，能力一律来自证据或探测。未命中 shipped
//! 表不再等价于「能力全关」，而是 [`ProtocolProfile::unverified`]（尚无证据：可惰性
//! 探测 / 可复用账本）；[`ProtocolProfile::closed`] 只留给**明确不可用**
//! （连接不可达 / 平台不支持 / 探测明确失败）。

use crate::services::codex_desktop_ipc::types::{CapabilitySet, ProtocolError};
use std::collections::BTreeMap;

/// 已验证 Desktop build 的能力/版本基线；未命中 shipped 表由调用方按证据级别建
/// [`ProtocolProfile::unverified`] 或 [`ProtocolProfile::closed`]。
#[derive(Clone)]
pub struct ProtocolProfile {
    pub platform: String,
    pub build: String,
    pub capabilities: CapabilitySet,
    pub method_versions: BTreeMap<String, u32>,
    /// #172：这份 profile 是「**尚无证据**」（未知 build，可探测 / 可查账本）还是
    /// 「**明确不可用**」（连接不可达 / 平台不支持 / 探测明确失败）。
    /// 两者能力位都是全 false，但**能不能探**不同——这是从「准入白名单」降级为
    /// 「证据缓存」后唯一必须保留的区分。
    pub evidence: EvidenceLevel,
}

/// profile 的证据级别（不是用户可见档位；用户档位见 `ControlAvailability`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceLevel {
    /// 精确 build 在 shipped 表里（真机证据，无需探测）。
    Shipped,
    /// 未知 build：**尚无证据**，首次触发时惰性探测（或复用账本）。
    Unverified,
    /// 明确不可用：连接不可达 / 平台不支持 / 探测明确失败——不再探测。
    Closed,
}

impl ProtocolProfile {
    /// 能力全关的诚实缺省（R3 定案）：未识别 build / 无 endpoint / 连接失败时使用。
    /// caps 全 false（A 写全关、投影 CompatOnly），但保留标准方法表——只读同步仍可。
    pub fn closed() -> Self {
        Self {
            platform: std::env::consts::OS.to_string(),
            build: String::new(),
            capabilities: CapabilitySet {
                send: false,
                state: false,
                command: false,
                permissions: false,
                blocking_input: false,
                interrupt: false,
            },
            method_versions: method_table(),
            evidence: EvidenceLevel::Closed,
        }
    }

    /// #172：**「尚无证据」档**——「尚无证据」不是「明确不可用」。能力位全 false
    /// （未取证前不启用任何能力），方法表沿用基线；与 [`closed`] 的区别只在
    /// [`evidence`]：这一档**可以**惰性探测 / 复用账本，判死档不行。
    /// 未知 build 的生产档位已让位给 [`compat_send_profile`]（2026-10-22 用户
    /// 裁决）；本档保留给 debug 强制开关（`force_unverified`）压档调试探测分支。
    pub fn unverified(platform: &str, build: &str) -> Self {
        Self {
            platform: platform.to_string(),
            build: build.to_string(),
            capabilities: CapabilitySet {
                send: false,
                state: false,
                command: false,
                permissions: false,
                blocking_input: false,
                interrupt: false,
            },
            method_versions: method_table(),
            evidence: EvidenceLevel::Unverified,
        }
    }

    /// 是否处于「尚无证据、可探测」档（只有这一档允许触发只读探测）。
    pub fn is_probe_eligible(&self) -> bool {
        self.evidence == EvidenceLevel::Unverified
    }

    /// 按方法取版本。`envelope_host` 为 `Some` 时仅 `thread-follower-*` 基值 +1
    /// （本地 follower request envelope 不发 hostId，走基值——两种位置都要钉住）。
    pub fn version(&self, method: &str, envelope_host: Option<&str>) -> Result<u32, ProtocolError> {
        let base = *self
            .method_versions
            .get(method)
            .ok_or_else(|| ProtocolError::UnsupportedMethod(method.to_string()))?;
        if envelope_host.is_some() && method.starts_with("thread-follower-") {
            Ok(base + 1)
        } else {
            Ok(base)
        }
    }
}

/// 协议方法版本表（与平台无关；探测定案 + 计划「协议与工程值」）。
/// `load-complete-history` / command 变体 / Plan blocking input 在 Mac 未实测，
/// 版本常量按表登记但能力 flag 保持 false——未验证不启用。
///
/// **本表是版本号的唯一来源**：派发侧（`adapter::dispatch`）只按方法名查表拿版本，
/// 任何调用点都不得硬编码版本号；**新增方法必须先有真机证据**（官方方法表核对，
/// 见 `tests/fixtures/codex-desktop-ipc/observed/README.md`）才能进表——未进表的方法
/// 一律视为「明确方法缺失」，派发返回 `NotSent`，绝不带猜测版本瞎发。
fn method_table() -> BTreeMap<String, u32> {
    BTreeMap::from([
        ("thread-owner-discovery".to_string(), 1),
        ("thread-stream-following-changed".to_string(), 1),
        ("thread-stream-following-status-requested".to_string(), 1),
        ("thread-stream-state-changed".to_string(), 11),
        ("thread-follower-start-turn".to_string(), 2),
        ("thread-follower-load-complete-history".to_string(), 1),
        ("thread-follower-command-approval-decision".to_string(), 1),
        // 依据 = build `26.1016.6573.0` 真机方法表（2026-10-21 探测批，
        // observed/README.md「官方方法表与 26.1013.4894.0 完全一致」）：文件变更审批的
        // 决策方法与 command 决策方法同为 v1，是官方基线方法——此前遗漏导致 file 审批
        // 只能靠调用点硬编码版本，现纳入单一真相源。
        ("thread-follower-file-approval-decision".to_string(), 1),
        (
            "thread-follower-permissions-request-approval-response".to_string(),
            1,
        ),
        ("thread-follower-submit-user-input".to_string(), 1),
        // 打断桌面当前回合（2026-10-21 由 Desktop 侧方法表静态取证：方法名存在、
        // 基值 4；params `{conversationId, mode, expectedTurnId}`；被打断的回合
        // 状态置 `interrupted`）。**必须带 expectedTurnId**——Desktop 的版本协商
        // 对「缺 expectedTurnId 的 interrupt」期望版本 3，带上则走基值 4。
        ("thread-follower-interrupt-turn".to_string(), 4),
        // broadcast/信号面（接收为主）
        ("client-status-changed".to_string(), 0),
        ("ipc-connection-reset".to_string(), 1),
    ])
}

/// Windows 基线（精确 build `26.1013.3656.0`）：command accept/decline 与 Plan
/// blocking input 在 **Windows Probe**（2026-10-16 探测批；spec「已验证版本」表登记
/// 的 Windows build 正是 3656.0）实测 PASS，permissions 未实测 → false。
/// `26.1013.4894.0` 已于 2026-10-19 真机探测批凭自有证据单独登记（见
/// [`windows_4894_profile`]），不与 3656.0 共用证据链。
fn windows_profile(build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: "windows".to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: true,
            permissions: false,
            blocking_input: true,
            interrupt: false,
        },
        method_versions: method_table(),
        evidence: EvidenceLevel::Shipped,
    }
}

/// macOS 基线：permissions / start-turn / following 已实测 PASS；command 变体与
/// blocking input 未实测（NOT_TRIGGERED），flag 保持 false。
fn macos_profile(build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: "macos".to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: true,
            blocking_input: false,
            interrupt: false,
        },
        method_versions: method_table(),
        evidence: EvidenceLevel::Shipped,
    }
}

/// macOS 登记档（interrupt 开）——三 build 能力面相同，共享构造器；逐 build 的
/// 取证故事见 `profile_for` 各臂注释与对应 reprobe 文档。共性纪律：**审批/问答/
/// permissions 未实测不跨 build 迁移**（旧 build 的 PASS 不自带）。
///
/// - `26.1013.40934`（2026-10-18 实机登记）：只读方法 + start-turn v2 受控 harness
///   实证（initialize/discovery/following/snapshot/load-complete-history 复审探针
///   验证；start-turn 实现侧探针实证——turn 成功创建 inProgress）。
/// - **interrupt（2026-10-21 起，三 build 同契约）**：Desktop 侧方法表基值 4，
///   params `{conversationId, mode, expectedTurnId}`，被打断回合置 `interrupted`；
///   配合 `turn_is_terminal` 放行后「打断 → 终态 → 屏障释放 → 队列接力」闭环。
/// - `26.1016.51333` / `26.1016.61520`（2026-10-22 两轮复探）：四项静态证据
///   （方法表 / 版本协商 / interrupt handler / 安全阀）逐字符零漂移 + 活体探针同构。
fn macos_interrupt_profile(build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: "macos".to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
            interrupt: true,
        },
        method_versions: method_table(),
        evidence: EvidenceLevel::Shipped,
    }
}

/// Windows `26.1013.4894.0`（2026-10-19 真机探测批登记）：受控探针 prodprobe3 直连
/// 真实 ChatGPT.exe 管道全链取证——只读协议往返（T1）、start-turn v2 创建 Turn 至
/// completed（T4）、command approval accept/decline 双臂 + 文件落盘/未落盘（T6）、
/// `/plan` 阻塞 requestUserInput 原 id 作答（T8）、Desktop 重启恢复（T9，owner 换代、
/// 旧 epoch 帧未复用）；金样 5 条事实与 macOS 同构（T2）。**permissions 保持 false**：
/// T7 NOT_AVAILABLE——本 build 的读升权实际走 command approval，未出现
/// `item/permissions/requestApproval`，无证据不开。`method_versions` 无漂移。
/// 证据：脱敏夹具 `tests/fixtures/codex-desktop-ipc/observed/windows-4894-*.sanitized.json`
/// 与仓外报告 `tuvis-verify-logs/2026-10-19-4894/results.md`（能力×证据矩阵 §3、
/// 版本核对表 §3）。
fn windows_4894_profile(build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: "windows".to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: true,
            permissions: false,
            blocking_input: true,
            interrupt: false,
        },
        method_versions: method_table(),
        evidence: EvidenceLevel::Shipped,
    }
}

/// Windows `26.1016.6573.0`（2026-10-22 真机取证批登记）：Desktop 自动升级后的**当前**
/// build——四条证据齐备，逐条有真机依据：
/// * **读链**：生产代码路径只读探测 P0–P3 全通过（`ensure_read_evidence` → 真实
///   IPC：owner discovery → 临时 following → 首个 snapshot 判据）；
/// * **send**：真机 send 闭环（`start_turn_compat` 走生产 adapter 路径，**只发一帧**）→
///   回执 `Accepted`（674ms）+ 回执内 `turn.status="inProgress"`，事后核对**状态真的前进**
///   （revision 前进 / 消息变多）——「受理 ≠ 生效」这条纪律在 send 上同样成立并已核对；
/// * **command 审批**：真机 decision 闭环（`thread-follower-command-approval-decision`
///   v1 / decision=accept → `resultType=success`、`result.ok=true`），脱敏夹具
///   `observed/windows-6573-command-approval-accept.sanitized.json`；
/// * **官方方法表**：`app.asar` 里 `thread-stream-state-changed:11` / `start-turn:2` /
///   各 decision v1 与 `26.1013.4894.0` **逐项一致**（含
///   `thread-follower-file-approval-decision:1`）→ 协议语法未漂移，4894 上的 decline 可用性
///   可平移。
///
/// **保持关闭的位（如实）**：`permissions`（无 `item/permissions/requestApproval` 证据）、
/// `blocking_input`（本 build 未实测 Plan 阻塞问答）、Plan / collaboration 模式切换。
fn windows_6573_profile(build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: "windows".to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: true,
            permissions: false,
            blocking_input: false,
            interrupt: false,
        },
        method_versions: method_table(),
        evidence: EvidenceLevel::Shipped,
    }
}

/// 精确 build 匹配（不是版本下限——升级后的 build 未经验证必须重新门控）。
pub fn profile_for(platform: &str, build: &str) -> Option<ProtocolProfile> {
    match (platform, build) {
        ("windows", "26.1013.3656.0") => Some(windows_profile(build)),
        ("windows", "26.1013.4894.0") => Some(windows_4894_profile(build)),
        ("windows", "26.1016.6573.0") => Some(windows_6573_profile(build)),
        ("macos", "26.1013.31256") => Some(macos_profile(build)),
        // 40934：2026-10-18 实机登记 + 2026-10-21 interrupt 取证
        //（docs/superpowers/reviews/2026-10-22-codex-interrupt-probe-results.md）
        ("macos", "26.1013.40934") => Some(macos_interrupt_profile(build)),
        // 51333：2026-10-22 复探零漂移（reviews/2026-10-22-codex-app-51333-reprobe.md）
        ("macos", "26.1016.51333") => Some(macos_interrupt_profile(build)),
        // 61520：2026-10-22 深夜复探零漂移（reviews/2026-10-22-codex-app-61520-reprobe.md）
        ("macos", "26.1016.61520") => Some(macos_interrupt_profile(build)),
        _ => None,
    }
}

/// 未知 build 的**兼容发送档**（2026-10-22 用户裁决「build 没对上时默认开会话
/// 通道」）：send/state 开，破坏性能力（interrupt / command / permissions /
/// blocking_input）仍走证据门控不随档放开。漂移兜底链：方法版本不匹配 →
/// `request-version-mismatch` → NotSent 如实未送达；回合卡死 → 镜像可见 +
/// 原生回合打断出口（B3）。登记 build 的证据门控不受影响——本档只接住
/// `profile_for` 落空的未知 build；空 build（Desktop 身份读不出来）仍用 closed。
///
/// `evidence = Unverified`（#172 接线，合并裁决）：本档保持**可探测**——只读探测 /
/// 账本 / schema 证据照常运转，command/permissions 等证据门控能力在取证后解锁。
pub fn compat_send_profile(platform: &str, build: &str) -> ProtocolProfile {
    ProtocolProfile {
        platform: platform.to_string(),
        build: build.to_string(),
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
            interrupt: false,
        },
        method_versions: method_table(),
        evidence: EvidenceLevel::Unverified,
    }
}

/// 连接档位选择：登记 build → 证据档；未知 build（非空）→ 兼容发送档（用户
/// 裁决，见 [`compat_send_profile`]）；空 build（身份读不出来/多义）→ closed。
pub fn profile_for_or_compat(platform: &str, build: &str) -> ProtocolProfile {
    match profile_for(platform, build) {
        Some(p) => p,
        None if !build.is_empty() => compat_send_profile(platform, build),
        None => ProtocolProfile::closed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::types::PendingRequestId;

    #[test]
    fn closed_profile_has_zero_caps_but_standard_method_table() {
        let c = ProtocolProfile::closed();
        assert!(!c.capabilities.send, "closed：A 写全关");
        assert!(!c.capabilities.state);
        assert!(!c.capabilities.command);
        assert!(!c.capabilities.permissions);
        assert!(!c.capabilities.blocking_input);
        // 只读同步仍可：标准方法表在场
        assert_eq!(c.version("thread-owner-discovery", None).unwrap(), 1);
        assert_eq!(c.version("thread-stream-state-changed", None).unwrap(), 11);
        assert_eq!(
            c.version("thread-follower-load-complete-history", None)
                .unwrap(),
            1
        );
    }

    /// 打断桌面当前回合（2026-10-21）：方法已登记且版本与 Desktop 侧方法表一致
    /// （基值 4）；能力位**按 build 逐个门控**——未实测的 build 不开。
    #[test]
    fn interrupt_turn_registered_v4_and_capability_gated_per_build() {
        // 方法已登记：本地 follower 请求（envelope 不发 hostId）→ 基值 4
        let mac = profile_for("macos", "26.1013.40934").unwrap();
        assert_eq!(
            mac.version("thread-follower-interrupt-turn", None).unwrap(),
            4,
            "打断方法版本与 Desktop 侧方法表基值一致"
        );
        assert!(mac.capabilities.interrupt, "40934 build 已开启打断能力");
        // 未实测的 build 一律不开（不盲发）
        let mac_old = profile_for("macos", "26.1013.31256").unwrap();
        assert!(!mac_old.capabilities.interrupt);
        let win = profile_for("windows", "26.1013.4894.0").unwrap();
        assert!(!win.capabilities.interrupt);
        let win_base = profile_for("windows", "26.1013.3656.0").unwrap();
        assert!(!win_base.capabilities.interrupt);
        // closed profile：能力全关但方法表仍在（只读同步不受影响）
        let closed = ProtocolProfile::closed();
        assert!(!closed.capabilities.interrupt);
        assert_eq!(
            closed
                .version("thread-follower-interrupt-turn", None)
                .unwrap(),
            4
        );
    }

    #[test]
    fn unverified_or_unknown_builds_are_none() {
        assert!(profile_for("macos", "26.1013.31257").is_none());
        assert!(profile_for("macos", "26.1013.40935").is_none());
        // 40934 精确命中：send/state 开（harness 实证），审批/问答关（未实测）
        let p = profile_for("macos", "26.1013.40934").expect("registered");
        assert!(p.capabilities.send && p.capabilities.state);
        assert!(!p.capabilities.permissions);
        assert!(!p.capabilities.command);
        assert!(!p.capabilities.blocking_input);
        // 51333 = 2026-10-22 复探登记（方法表/协商/安全阀零漂移，见
        // docs/superpowers/reviews/2026-10-22-codex-app-51333-reprobe.md）：
        // caps 与 40934 同面；相邻 build 仍 fail-closed
        let p51333 = profile_for("macos", "26.1016.51333").expect("registered");
        assert!(p51333.capabilities.send && p51333.capabilities.state);
        assert!(p51333.capabilities.interrupt);
        assert!(!p51333.capabilities.permissions);
        assert!(!p51333.capabilities.command);
        assert!(!p51333.capabilities.blocking_input);
        assert!(profile_for("macos", "26.1016.51334").is_none());
        // 61520 = 2026-10-22 深夜复探登记（当日第二次自动升级，零漂移，
        // 见 docs/superpowers/reviews/2026-10-22-codex-app-61520-reprobe.md）
        let p61520 = profile_for("macos", "26.1016.61520").expect("registered");
        assert!(p61520.capabilities.send && p61520.capabilities.state);
        assert!(p61520.capabilities.interrupt);
        assert!(!p61520.capabilities.permissions);
        assert!(!p61520.capabilities.command);
        assert!(!p61520.capabilities.blocking_input);
        assert!(profile_for("macos", "26.1016.61521").is_none());
        assert!(profile_for("windows", "26.1013.3655.0").is_none());
        // 4894.0 = 2026-10-19 真机探测批登记（T1/T4/T6/T8/T9 PASS + T2 同构；证据 =
        // observed/windows-4894-*.sanitized.json）：caps 逐项钉死，permissions 无证据保持关
        let p4894 = profile_for("windows", "26.1013.4894.0").expect("registered");
        assert!(p4894.capabilities.send && p4894.capabilities.state);
        assert!(p4894.capabilities.command && p4894.capabilities.blocking_input);
        assert!(!p4894.capabilities.permissions);
        // 未探测的相邻 build 仍然 fail-closed（精确门，不是版本下限）
        assert!(profile_for("windows", "26.1013.4895.0").is_none());
        // 6573.0 = 2026-10-22 真机取证批登记（读链 P0–P3 + send 闭环 + command 审批
        // 闭环 + 官方方法表与 4894 逐项一致）：caps 逐项钉死；permissions / blocking_input
        // 无证据保持关
        let p6573 = profile_for("windows", "26.1016.6573.0").expect("registered");
        assert!(p6573.capabilities.send && p6573.capabilities.state);
        assert!(p6573.capabilities.command);
        assert!(!p6573.capabilities.permissions);
        assert!(
            !p6573.capabilities.blocking_input,
            "6573 未实测 Plan 阻塞问答 → 保持关（如实）"
        );
        assert!(profile_for("windows", "26.1016.6573.1").is_none());
        assert!(profile_for("linux", "26.1013.31256").is_none());
        assert!(profile_for("macos", "").is_none());
        // 已验证 build 精确命中
        assert!(profile_for("macos", "26.1013.31256").is_some());
        assert!(profile_for("windows", "26.1013.3656.0").is_some());
    }

    /// 未知 build 兜底档（2026-10-22 用户裁决「build 没对上默认开会话通道」）：
    /// 未知 build → 兼容发送档（send/state 开，破坏性能力保持证据门关）；
    /// 空 build（身份读不出来/多义）→ closed；登记 build → 证据档原样。
    #[test]
    fn profile_for_or_compat_unknown_build_defaults_compat_send() {
        let p = profile_for_or_compat("macos", "26.1016.99999");
        assert!(
            p.capabilities.send && p.capabilities.state,
            "会话通道默认开"
        );
        assert!(!p.capabilities.interrupt, "破坏性能力不随档放开");
        assert!(!p.capabilities.command && !p.capabilities.permissions);
        assert!(!p.capabilities.blocking_input);
        assert_eq!(p.build, "26.1016.99999", "build 原样保留（归因/缓存键）");
        let w = profile_for_or_compat("windows", "26.1020.7000.0");
        assert!(w.capabilities.send && w.capabilities.state);
        assert!(!w.capabilities.interrupt);
        // 空 build（desktop_build 多义/读不出）→ closed 保守不变
        let empty = profile_for_or_compat("macos", "");
        assert!(!empty.capabilities.send, "空 build 仍 closed");
        // 登记 build → 证据档原样（不打折）
        let reg = profile_for_or_compat("macos", "26.1016.61520");
        assert!(reg.capabilities.interrupt, "登记 build 能力面原样");
    }

    #[test]
    fn version_gate_base_and_envelope_host_plus_one() {
        let p = profile_for("macos", "26.1013.31256").unwrap();
        // 本地 follower request：envelope 不发 hostId → 基值
        assert_eq!(p.version("thread-follower-start-turn", None).unwrap(), 2);
        // 构造器显式携带 envelope hostId：仅 follower 方法按基值 +1
        assert_eq!(
            p.version("thread-follower-start-turn", Some("local"))
                .unwrap(),
            3
        );
        // 非 follower 方法不受 envelope hostId 影响
        assert_eq!(p.version("thread-stream-state-changed", None).unwrap(), 11);
        assert_eq!(
            p.version("thread-stream-state-changed", Some("local"))
                .unwrap(),
            11
        );
        assert_eq!(p.version("client-status-changed", None).unwrap(), 0);
        // 未登记方法拒绝，不盲发。
        // （反例名用**确实不存在**的方法——`thread-follower-interrupt-turn` 已于
        //  2026-10-21 登记为打断方法（基值 4），不能再当未登记反例。）
        assert_eq!(
            p.version("thread-follower-not-a-real-method", None),
            Err(ProtocolError::UnsupportedMethod(
                "thread-follower-not-a-real-method".to_string()
            ))
        );
    }

    /// T7-R3：`26.1016.6573.0` 真机方法表取证——文件变更审批的决策方法必须进表
    /// （与 command 决策方法并列，均为 v1）。此前漏登记，adapter 只能硬编码版本。
    #[test]
    fn method_table_registers_file_approval_decision_from_observed_build() {
        let table = method_table();
        assert_eq!(
            table.get("thread-follower-file-approval-decision"),
            Some(&1),
            "官方方法表（26.1016.6573.0 真机核对）登记 file 决策方法 v1"
        );
        assert_eq!(
            table.get("thread-follower-command-approval-decision"),
            Some(&1),
            "command 决策方法仍并列在表（两条决策方法互不替代）"
        );
        // 公开查表口径（envelope 不含 hostId → 基值）：file 决策方法可解析到 v1
        let p = ProtocolProfile::closed();
        assert_eq!(
            p.version("thread-follower-file-approval-decision", None)
                .unwrap(),
            1
        );
    }

    #[test]
    fn profiles_keep_unverified_methods_disabled() {
        let mac = profile_for("macos", "26.1013.31256").unwrap();
        assert_eq!(mac.platform, "macos");
        assert!(mac.capabilities.send);
        assert!(mac.capabilities.state);
        assert!(mac.capabilities.permissions);
        assert!(!mac.capabilities.command);
        assert!(!mac.capabilities.blocking_input);

        let win = profile_for("windows", "26.1013.3656.0").unwrap();
        assert!(win.capabilities.send);
        assert!(win.capabilities.state);
        assert!(win.capabilities.command);
        assert!(win.capabilities.blocking_input);
        assert!(!win.capabilities.permissions);

        let win4894 = profile_for("windows", "26.1013.4894.0").unwrap();
        assert!(win4894.capabilities.send);
        assert!(win4894.capabilities.state);
        assert!(win4894.capabilities.command);
        assert!(win4894.capabilities.blocking_input);
        assert!(!win4894.capabilities.permissions);
    }

    #[test]
    fn request_id_key_shapes() {
        assert_eq!(
            PendingRequestId::from_key("n:9007199254740993")
                .unwrap()
                .to_key(),
            "n:9007199254740993"
        );
        assert_ne!(
            PendingRequestId::from_key("n:12").unwrap(),
            PendingRequestId::from_key("s:12").unwrap()
        );
    }
}

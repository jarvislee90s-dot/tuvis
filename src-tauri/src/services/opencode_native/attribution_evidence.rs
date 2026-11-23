//! N2/M2 语义证据准入：echo-challenge 归属机制的**版本绑定**（H02 结论速览 9 判据
//! 定义）。
//!
//! 机制前提「TUI 数据层缓存只随 SSE 失效事件刷新、失效为 per-session」是
//! **v2.2.5 的实测行为事实，不是任何正式契约**（H02 §8 边界③、结论速览 9）：上游可
//! 静默改变；单独失效 = fail-closed 可用性损失，与语义漂移叠加才是错误放行方向——
//! 故本模块把「当前 OpenCode 构建 / 平台 / 启动形态组合是否已有登记的归属行为证据」
//! 变成共享写权入口（`verify_write_authority`）的**第一道核对**（先于探针创建/改写）：
//!
//! * 组合已登记 → 继续逐动作走原有绑定核对 / 回声门 / ACK 终验（准入只是第一道，
//!   不替代任何门内判据）；
//! * 组合未登记（或锚定 TUI 身份 / 任一构建 SHA 取不到）→ `OC_GATE_SEMANTICS_
//!   UNPROVEN` 受控拒绝（三零：零探针写、零动作行、零队列项）。原生只读、旧动作
//!   只读核对与本地安全操作（暂停/撤回本地队列项）不过本门，照常可用。
//!
//! ## 登记组合的两个构建维度（M2，复审「准入只验了一半」）
//!
//! 归属语义（数据层 SSE 失效刷新 → 插件 ack 回写）由 **TUI 与服务共同表现**：
//! TUI 数据层决定 challenge 是否可见，服务进程承载事件总线与 HTTP 写路径——只验
//! TUI 构建会放过「TUI 是登记构建、服务是未知构建」的组合。故登记条目与准入判定
//! 都核对**两个构建**：锚定 TUI 的二进制 SHA-256 + 服务进程的二进制 SHA-256
//!（H02 取证拓扑中两者是同一 `opencode` 二进制，逐字同值）。
//!
//! ## 登记表与升级复验流程（可执行清单，H02 §12 判据锚定）
//!
//! 登记条目 = (锚定 TUI 构建 SHA-256, 服务构建 SHA-256, 平台, 启动形态集合, 证据
//! 指针, 取证日期)。**升级或变更组合时，复验全部通过后才可登记/续登，未复验前
//! 写入保持拒绝**：
//!
//! 1. 取新二进制 SHA-256（`shasum -a 256 <opencode 二进制>`；隔离环境内目标）——
//!    **TUI 与服务分别锚定**（同二进制部署时同值，仍须各自实算并如实记录）；
//! 2. 重跑 **B 正向放行**（H02 E2：challenge 经 TUI 实际连接的实例 → 窗口内锁定
//!    pid 的合法 ack）；
//! 3. 重跑 **旧 A 存活拒绝**（H02 E3：TUI 已迁 B 时，经仍存活的旧 A 的 challenge
//!    → 窗口内零 ack）；
//! 4. 重跑 **per-session 失效粒度**（H02 §12 E1c：他会话的失效事件不触发探针键
//!    刷新，challenge 形态误火探针零 ack）——失效粒度变粗是唯一错误放行方向，
//!    锚定二进制后必须重跑关闭；
//! 5. 以上全部 PASS → 在 [`registry`] 增补/更新条目（TUI/服务 SHA/平台/形态/
//!    证据指针/日期）；任一步 FAIL → 不登记，`OC_GATE_SEMANTICS_UNPROVEN` 保持
//!    拒绝。
//!
//! 纪律：SHA 计算**只读**目标二进制文件；登记表是编译期常量（登记 = 改本文件 +
//! 证据文档，随评审入库），不落用户目录。

use std::sync::Arc;

use super::types::{codes, NativeError};

/// 启动形态词：托管（登记文件发现的后台 service）。
pub(crate) const LAUNCH_MANAGED: &str = "managed";
/// 启动形态词：显式连接（argv `--server`+`--session`，无登记文件）。
pub(crate) const LAUNCH_EXPLICIT: &str = "explicit";

/// 首条登记的锚定构建（H02，v2.2.5；与证据文档 §0 实测 SHA 逐字一致）。
/// pub：集成夹具与升级复验清单都以它为对照锚。H02 取证拓扑中**服务与 TUI 是同一
/// 二进制**（单一 `opencode` 可执行），登记条目的两个构建维度共用此值。
pub const H02_ANCHOR_SHA256: &str =
    "abe4d88594bffb12b364375b8a6732d2d1f7a977195ed1a35c5c76f8e89c9e0e";

/// 平台词（编译期事实；与登记条目的 `platform` 字段精确匹配）。
pub(crate) fn platform_word() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// 一条已证的归属行为语义证据（H02 判据锚定；M2 起含服务构建维度）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttributionEvidenceEntry {
    /// 锚定 TUI 构建 SHA-256（小写 hex，64 位；`shasum -a 256` 口径）。
    pub tui_sha256: &'static str,
    /// 服务构建 SHA-256（小写 hex；经服务进程可执行路径解析的同一口径）。
    pub service_sha256: &'static str,
    /// 取证平台（[`platform_word`] 同词表）。
    pub platform: &'static str,
    /// 已证启动形态集合（[`LAUNCH_MANAGED`] / [`LAUNCH_EXPLICIT`]）。
    pub launch_forms: &'static [&'static str],
    /// 证据文档指针（仓库相对路径；取证场景 = 托管/显式/分裂脑拓扑）。
    pub evidence: &'static str,
    /// 取证日期（ISO 8601；升级复验后刷新）。
    pub evidence_date: &'static str,
}

/// 已证语义证据登记表（**唯一登记处**；新组合必须在隔离环境完成上节复验清单并
/// 连同证据文档入库后才可增补）。
pub(crate) fn registry() -> &'static [AttributionEvidenceEntry] {
    &[AttributionEvidenceEntry {
        tui_sha256: H02_ANCHOR_SHA256,
        // 首条登记的服务构建 = 同一二进制（H02 取证：隔离服务与 TUI 均为
        // abe4d885…c9e0e；证据文档 §0/§10 进程清单可复核）。
        service_sha256: H02_ANCHOR_SHA256,
        platform: "macos",
        launch_forms: &[LAUNCH_MANAGED, LAUNCH_EXPLICIT],
        evidence:
            "docs/engineering/evidence/opencode-native/2026-11-14-h02-echo-challenge-attribution.md",
        evidence_date: "2026-11-14",
    }]
}

/// 准入判定（纯函数）：平台、启动形态、锚定 TUI 构建 SHA 与服务构建 SHA 四者都对上
/// **同一条**登记才算适用（精确匹配；SHA 大小写敏感——解析器契约 = 小写 hex，非
/// 小写即视为未登记，fail-closed）。
pub(crate) fn admitted(
    platform: &str,
    launch_form: &str,
    tui_sha256: &str,
    service_sha256: &str,
) -> bool {
    registry().iter().any(|entry| {
        entry.tui_sha256 == tui_sha256
            && entry.service_sha256 == service_sha256
            && entry.platform == platform
            && entry.launch_forms.contains(&launch_form)
    })
}

/// 准入拒绝的受控错误（固定文案；零凭据、零原始响应体、零 SHA 实参）。
pub(crate) fn unproven_error() -> NativeError {
    NativeError::new(
        codes::OC_GATE_SEMANTICS_UNPROVEN,
        "this opencode build has no registered ownership semantics evidence",
    )
}

/// 锚定 TUI 构建解析缝：pid → 二进制 SHA-256（小写 hex；`None` = 取不到，fail-closed）。
/// 生产实现见 [`production_tui_binary_sha`]；测试注入桩（与 `TuiProcessFact` 同级缝）。
pub type TuiBinarySha = Arc<dyn Fn(u32) -> Option<String> + Send + Sync>;

/// M2 服务构建解析缝：服务进程 pid → 二进制 SHA-256（小写 hex；`None` = 取不到，
/// fail-closed）。生产实现与 TUI 共用 [`production_tui_binary_sha`]（该函数按 pid
/// 泛用：sysinfo 解析目标进程可执行路径后流式哈希，只读目标二进制文件）；装门
/// 测试注入桩与 [`TuiBinarySha`] 同级同形。
pub type ServiceBinarySha = Arc<dyn Fn(u32) -> Option<String> + Send + Sync>;

/// 生产解析：pid →（sysinfo）可执行路径 → 流式 SHA-256 → 小写 hex。
///
/// 只读目标二进制文件（进程存活期内容不变）；任何一步拿不到（进程已死 / 无 exe /
/// 读失败）→ `None`，调用方按不适用处理（fail-closed），不猜版本号。
/// SHA 计算每代际一次（缓存见 projection 的 `semantic_sha_cache` / `service_sha_
/// cache`）；本函数自身无缓存、无副作用、不读二进制以外的任何内容。
pub(crate) fn production_tui_binary_sha(pid: u32) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;

    let sys_pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[sys_pid]), true);
    let exe = system.process(sys_pid)?.exe()?.to_path_buf();
    let mut file = std::fs::File::open(exe).ok()?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    let digest = hasher.finalize();
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登记表非空后的首条登记锚（H02：abe4d885…c9e0e / macOS / managed+explicit /
    /// 2026-11-14）。RED 期登记表为空 → 本用例与正向准入用例全红。
    const H02_SHA: &str = H02_ANCHOR_SHA256;
    /// 同版本不同构建（sha 仅尾段不同；HTTP 契约层面与登记样本不可区分）。
    const OTHER_BUILD_SHA: &str =
        "abe4d88594bffb12b364375b8a6732d2d1f7a977195ed1a35c5c76f8e89c9e0d";

    #[test]
    fn first_registered_entry_anchors_the_h02_evidence() {
        let entries = registry();
        assert!(
            !entries.is_empty(),
            "语义证据登记表不得长期为空：首条 = H02 锚定构建"
        );
        let first = &entries[0];
        assert_eq!(first.tui_sha256, H02_SHA, "首条 TUI 构建 = H02 锚定二进制");
        assert_eq!(
            first.service_sha256, H02_SHA,
            "首条服务构建 = 同一二进制（H02 取证拓扑：服务与 TUI 均为 abe4d885…c9e0e）"
        );
        assert_eq!(first.platform, "macos", "H02 取证平台 = macOS");
        assert!(
            first.launch_forms.contains(&LAUNCH_MANAGED)
                && first.launch_forms.contains(&LAUNCH_EXPLICIT),
            "H02 覆盖托管 + 显式两种启动形态（E6 显式正例）"
        );
        assert!(
            first.evidence.contains("h02-echo-challenge-attribution"),
            "证据指针 = H02 证据文档"
        );
        assert_eq!(first.evidence_date, "2026-11-14");
    }

    #[test]
    fn registered_combination_is_admitted_for_both_launch_forms() {
        assert!(admitted("macos", LAUNCH_MANAGED, H02_SHA, H02_SHA));
        assert!(admitted("macos", LAUNCH_EXPLICIT, H02_SHA, H02_SHA));
    }

    #[test]
    fn same_version_different_build_is_not_admitted() {
        // 「HTTP schema 合规 ≠ 归属语义适用」的判据本体：契约与版本完全一致，
        // 只有构建 SHA 不同 → 不适用。M2：两个构建维度各自独立成立——只换其一
        // 即未登记组合。
        assert!(
            !admitted("macos", LAUNCH_MANAGED, OTHER_BUILD_SHA, H02_SHA),
            "只换 TUI 构建 → 拒"
        );
        assert!(
            !admitted("macos", LAUNCH_MANAGED, H02_SHA, OTHER_BUILD_SHA),
            "只换服务构建 → 拒（M2 补全的维度）"
        );
        assert!(
            !admitted("macos", LAUNCH_MANAGED, OTHER_BUILD_SHA, OTHER_BUILD_SHA),
            "双换 → 拒"
        );
    }

    #[test]
    fn other_platforms_and_unknown_forms_are_not_admitted() {
        assert!(!admitted("windows", LAUNCH_MANAGED, H02_SHA, H02_SHA));
        assert!(!admitted("linux", LAUNCH_EXPLICIT, H02_SHA, H02_SHA));
        assert!(!admitted("macos", "standalone", H02_SHA, H02_SHA));
        assert!(!admitted("macos", LAUNCH_MANAGED, "", H02_SHA));
        assert!(!admitted("macos", LAUNCH_MANAGED, H02_SHA, ""));
    }

    #[test]
    fn sha_shapes_other_than_lowercase_hex_are_not_admitted() {
        // 解析器契约 = 小写 hex；大写/带前缀/错长一律视为未登记（fail-closed，不归一化）。
        assert!(!admitted(
            "macos",
            LAUNCH_MANAGED,
            &H02_SHA.to_uppercase(),
            H02_SHA
        ));
        assert!(!admitted(
            "macos",
            LAUNCH_MANAGED,
            &format!("sha256:{H02_SHA}"),
            H02_SHA
        ));
        assert!(!admitted(
            "macos",
            LAUNCH_MANAGED,
            H02_SHA,
            &H02_SHA.to_uppercase()
        ));
        assert!(!admitted(
            "macos",
            LAUNCH_MANAGED,
            H02_SHA,
            &format!("sha256:{H02_SHA}")
        ));
    }

    #[test]
    fn unproven_error_carries_the_stable_reason_code() {
        let error = unproven_error();
        assert_eq!(error.code, codes::OC_GATE_SEMANTICS_UNPROVEN);
        assert!(!error.message.contains("abe4d885"), "文案不带 SHA 实参");
    }

    #[test]
    fn production_resolver_hashes_the_running_binary_as_lowercase_hex() {
        let sha = production_tui_binary_sha(std::process::id())
            .expect("自身进程的可执行文件必须可读可哈希（只读目标二进制）");
        assert_eq!(sha.len(), 64, "SHA-256 hex 长度：{sha}");
        assert!(
            sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "解析器契约 = 小写 hex：{sha}"
        );
        // 稳定性：同代际两次解析逐字一致（进程存活期二进制不变）。
        assert_eq!(
            production_tui_binary_sha(std::process::id()),
            Some(sha.clone())
        );
        // 服务构建解析共用同一实现（pid 泛用；自身进程即「服务进程」的等价锚）。
        let service_sha = production_tui_binary_sha(std::process::id())
            .expect("服务进程可执行路径解析 = 同一生产实现");
        assert_eq!(service_sha, sha, "同一进程两次解析同值（TUI/服务共用实现）");
    }
}

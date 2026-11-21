//! 签发（BindingBook）与计划器（planner）共用的 v2 卡面指纹规范。
//!
//! v2 规范（固定 canonical bytes，禁止改动字段顺序/编码/标签）：
//! ```text
//! hex(SHA256(serde_json 紧凑 UTF-8 tuple(
//!   "codex-question-card/v2", title, ordered_options,
//!   native_question_count, "explicit-user-full-panel-count", lease_id,
//!   (pid, started_at_ms, hwnd, package_fingerprint, view_path, scope_epoch),
//!   full_card_runtime_id, full_dismiss_runtime_id
//! )))
//! ```
//!
//! - serde_json 将 tuple 序列化为紧凑 JSON 数组；`usize`/`u64`/`isize`/`u32` 均为数字，
//!   `Vec<Vec<i32>>` 为嵌套数组。固定测试向量：
//!   `["codex-question-card/v2","Synthetic question",["Alpha","Beta"],1,
//!     "explicit-user-full-panel-count","lease-a",
//!     [42,100,123,"package-a",[[1,2],[3,4]],7],[10,20],[10,20,30]]`
//!   其 SHA-256 为 `d1311f56527c014c2a69100c0f7b033ddb72a2bb24d4190d338029ec74e682fd`。
//! - count / 证据 / scope / button 全部进入指纹：`lease_id` 是不可变 CountEvidence 的唯一
//!   引用，换证据必须换 lease_id（即换指纹），不得原地更新。旧 v1（仅题文 tuple）不兼容，
//!   内存租约无需迁移。
//! - 序列化失败显式返回 `None`，由调用方保守拒绝；禁止 `unwrap_or_default` 产生空指纹。
//! - 旧 AnswerWitness / B1.3 的既有哈希规范与本 helper 无关，不得混用。
//!
//! 本文件另含 Windows 离线纯校验缝（M2/M3/M4/M5/M6/M7）：Fake/离线测试与 Windows
//! adapter 共用同一判定；全部缝已被 windows.rs 真实消费（audit/分类/基线/清单/长度）。

use std::time::Instant;

use sha2::{Digest, Sha256};

use super::types::{UiError, WindowScope};

/// v2 规范标签：与 v1（仅题文 tuple）严格区分。
pub(crate) const V2_TAG: &str = "codex-question-card/v2";
/// v2 题数证据来源标签：仅显式用户完整面板确认这一种来源。
pub(crate) const V2_COUNT_SOURCE: &str = "explicit-user-full-panel-count";

/// 计算 v2 卡面指纹；输入与计划器守卫和签发侧（BindingBook）完全同一规范。
///
/// - `title` / `ordered_options`：实测卡面题文（调用方须已与来源成员核对一致）；
/// - `confirmed_native_question_count`：已确认的原生完整面板题数（当前规范仅 1 可签发）；
/// - `lease_id`：不可变 CountEvidence 的唯一引用；
/// - `scope`：已核对一致的完整窗口作用域（含 view_path / scope_epoch）；
/// - `card_runtime_id` / `dismiss_runtime_id`：实测完整 RuntimeId。
///
/// 返回 `None` 表示序列化失败（显式保守拒绝路径）。
pub(crate) fn card_fingerprint_v2(
    title: &str,
    ordered_options: &[String],
    confirmed_native_question_count: usize,
    lease_id: &str,
    scope: &WindowScope,
    card_runtime_id: &[i32],
    dismiss_runtime_id: &[i32],
) -> Option<String> {
    let payload = serde_json::to_vec(&(
        V2_TAG,
        title,
        ordered_options,
        confirmed_native_question_count,
        V2_COUNT_SOURCE,
        lease_id,
        (
            scope.pid,
            scope.started_at_ms,
            scope.hwnd,
            scope.package_fingerprint.as_str(),
            scope.view_path.as_slice(),
            scope.scope_epoch,
        ),
        card_runtime_id,
        dismiss_runtime_id,
    ))
    .ok()?;
    let digest = Sha256::digest(&payload);
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

// ---------------------------------------------------------------------------
// Windows 离线纯校验缝（task1-specfix-handoff §文件级计划：无 COM 事实校验，
// Fake/离线测试与 Windows adapter 共用同一判定；系统调用仍由唯一 Windows adapter 承担）
// ---------------------------------------------------------------------------

// ---- M2：枚举完整性审计 ----

/// 树遍历深度上限（超过即 incomplete，不得静默 complete）。
pub(crate) const MAX_TREE_DEPTH: usize = 32;
/// 单节点直属兄弟枚举上限。
pub(crate) const MAX_SIBLING_COUNT: usize = 64;

/// 深度审计：`depth > MAX_TREE_DEPTH` 拒绝（incomplete/Err，不静默完成）。
pub(crate) fn audit_depth(depth: usize) -> Result<(), UiError> {
    if depth > MAX_TREE_DEPTH {
        Err(UiError::Unavailable)
    } else {
        Ok(())
    }
}

/// 兄弟数量审计：超限拒绝。
pub(crate) fn audit_siblings(sibling_count: usize) -> Result<(), UiError> {
    if sibling_count > MAX_SIBLING_COUNT {
        Err(UiError::Unavailable)
    } else {
        Ok(())
    }
}

/// 遍历预算审计：预算耗尽（remaining == 0）拒绝；节点/文本预算共用。
pub(crate) fn audit_budget(remaining: usize) -> Result<(), UiError> {
    if remaining == 0 {
        Err(UiError::Unavailable)
    } else {
        Ok(())
    }
}

/// TreeWalker 步进语义（M2 专项）：必须区分「真实 COM 调用失败」与
/// 「S_OK + null（兄弟尽头）」——不能把一切 Err 折成尽头，也不能把尽头当失败。
/// F1 实装后生产路径由 windows.rs 的原始 vtable wrapper `walk_step_with` 直接给出
/// 同一三分类（`Result<Option<_>>`），不再经本两 bool 缝自证；本缝保留给离线/Fake
/// 测试共用同一判定语义（仅测试消费，故挂定向 allow）。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkerStep {
    /// 取到元素，继续遍历。
    Advanced,
    /// 调用成功但无更多兄弟（合法尽头，不是错误）。
    EndOfSiblings,
    /// 底层 COM 调用真实失败（incomplete/Err，绝不静默 complete）。
    ComFailed,
}

/// 纯分类：`underlying_call_failed` = 底层 HRESULT 是否真失败（区别于 S_OK+null 被
/// 高层包装成 Err）；`element_present` = 调用成功时是否返回了元素。
#[allow(dead_code)]
pub(crate) fn classify_walker_step(
    underlying_call_failed: bool,
    element_present: bool,
) -> WalkerStep {
    if underlying_call_failed {
        WalkerStep::ComFailed
    } else if element_present {
        WalkerStep::Advanced
    } else {
        WalkerStep::EndOfSiblings
    }
}

// ---- M3：身份基线（完整 FILETIME / 包 / 实际 image path / 代际 / window RID）----

/// 单进程身份事实：`creation_filetime` 为 GetProcessTimes 完整 creation FILETIME
/// （100ns 刻度私存，非毫秒近似）；`image_path` 为 QueryFullProcessImageNameW 实际值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentityFact {
    pub(crate) pid: u32,
    pub(crate) creation_filetime: u64,
    pub(crate) package_full_name: Option<String>,
    pub(crate) image_path: String,
}

/// 已证身份基线：宿主 + 渲染子进程（含 renderer→host 代际由调用方以事实链证明）
/// + 窗口完整 RuntimeId。固定包/ASAR 白名单比对仍由 Windows adapter 承担。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdentityBaseline {
    pub(crate) host: ProcessIdentityFact,
    pub(crate) renderer: Option<ProcessIdentityFact>,
    pub(crate) window_rid: Vec<i32>,
}

impl IdentityBaseline {
    /// 逐字段严格重验：任何字段漂移都拒绝——尤其「同毫秒不同 FILETIME」（FILETIME
    /// 为 100ns 刻度，代表不同进程实例）也必须拒绝，不得按毫秒近似放行。
    /// 空帧（无卡）时外父链/基线仍须实读核对（不因无候选跳过身份）。
    pub(crate) fn revalidate(&self, current: &IdentityBaseline) -> Result<(), UiError> {
        if *self == *current {
            Ok(())
        } else {
            Err(UiError::Unavailable)
        }
    }
}

/// 进程父链的单条边事实：`child(pid, 完整 creation FILETIME) -> parent(pid, 完整
/// creation FILETIME)`。FILETIME 为 100ns 刻度完整值（非毫秒近似），是同 PID 复用
/// 下区分不同进程实例的唯一证据；Windows adapter 有界取材并前后核 stamps 后提供。
#[derive(Clone, Debug)]
pub(crate) struct ParentHop {
    pub(crate) child_pid: u32,
    pub(crate) child_filetime: u64,
    pub(crate) parent_pid: u32,
    pub(crate) parent_filetime: u64,
}

/// Windows 路径组件的离线相等（大小写不敏感；Windows 文件系统语义）。
fn path_component_eq(left: &std::path::Component<'_>, right: &std::path::Component<'_>) -> bool {
    left.as_os_str().eq_ignore_ascii_case(right.as_os_str())
}

/// F7：目录组件包含校验——`image_path` 必须是绝对规范路径，其目录组件必须逐项
///（大小写不敏感）位于 `package_root` 之内，且包内还有实际文件名；文件名必须是
/// 可执行镜像（*.exe，进程镜像天然是 exe）。逐组件比较（非字符串 prefix）：
/// `C:\Apps\Pkg` 下的路径不会被 `C:\Apps\PkgEvil\…` 相似前缀碰瓷。任意同包字符串
///（resources.pri 等非 exe）与包根本身一律拒绝。
fn require_image_inside_package(package_root: &str, image_path: &str) -> Result<(), UiError> {
    let root = std::path::Path::new(package_root);
    let image = std::path::Path::new(image_path);
    // QueryFullProcessImageNameW / GetPackagePathByFullName 产物均为绝对路径；
    // 相对路径 = 非规范实测值 → 保守拒绝。
    if !root.is_absolute() || !image.is_absolute() {
        return Err(UiError::Unavailable);
    }
    let root_components: Vec<_> = root.components().collect();
    let image_components: Vec<_> = image.components().collect();
    // 包内必须有至少一层实际文件（image == 包根本身 / 更浅 → 拒绝）。
    if image_components.len() <= root_components.len() {
        return Err(UiError::Unavailable);
    }
    for (root_part, image_part) in root_components.iter().zip(image_components.iter()) {
        if !path_component_eq(root_part, image_part) {
            return Err(UiError::Unavailable);
        }
    }
    let file_name = image.file_name().ok_or(UiError::Unavailable)?;
    let file = file_name.to_string_lossy().to_ascii_lowercase();
    if !file.ends_with(".exe") {
        return Err(UiError::Unavailable);
    }
    Ok(())
}

/// F7：父链端点代际连接——从 renderer 事实出发，逐条消费与其 child 端点
/// `(pid, 完整 FILETIME)` 全等匹配的 edge，走到 host 事实；同 PID 不同 FILETIME
/// 不匹配（不同进程实例），父晚于子（FILETIME 比较）拒绝。未消费完的全部 hop、
/// 走不到 host、材料成环均 = 证据不足 → 拒绝。
fn connect_identity_chain(
    start: &ProcessIdentityFact,
    host: &ProcessIdentityFact,
    hops: &[ParentHop],
) -> Result<(), UiError> {
    let mut current = (start.pid, start.creation_filetime);
    let target = (host.pid, host.creation_filetime);
    let mut used = vec![false; hops.len()];
    for _ in 0..hops.len() {
        if current == target {
            break;
        }
        let mut matched: Option<(usize, (u32, u64))> = None;
        for (index, hop) in hops.iter().enumerate() {
            if used[index] {
                continue;
            }
            if (hop.child_pid, hop.child_filetime) == current {
                matched = Some((index, (hop.parent_pid, hop.parent_filetime)));
                break;
            }
        }
        let (index, parent) = matched.ok_or(UiError::Unavailable)?;
        used[index] = true;
        // 父不晚于子：父进程必须不迟于其后代创建（完整 FILETIME 比较）。
        if parent.1 > current.1 {
            return Err(UiError::Unavailable);
        }
        current = parent;
    }
    if current != target {
        return Err(UiError::Unavailable);
    }
    if used.iter().any(|consumed| !consumed) {
        // 剩余未消费的父链材料 = 与事实不符（含同 PID 不同 FILETIME 的重复端点）。
        return Err(UiError::Unavailable);
    }
    Ok(())
}

/// F7：身份链首次证明（每 observation 建立基线时调用，不用后续相等比较替代初次
/// 可信校验）。固定官方包关联由 Windows adapter 上游承担（pinned 包全名 + ASAR
/// SHA256 核对后传入实际包根），本函数做离线强校验：
/// 1. host/package 事实在场（缺包身份 = 缺证据，拒绝）；
/// 2. host/renderer 的 QueryFullProcessImageName 规范路径按【目录组件】位于包内
///    且为可执行镜像（错包 / 包外 / 相似前缀路径拒绝）；
/// 3. renderer 与 host 同包（错包拒绝）；renderer→host 父链每边 (pid, 完整
///    FILETIME) 真实可连到 host 原代际且父不晚于子（缺链 / 代际断裂 / 父比子新
///    / 同 PID 不同 FILETIME 拒绝；无 renderer 时父链材料必须为空）。
///
/// 返回基线的 `window_rid` 留空——由调用方以实读完整 RuntimeId 填充（helper 无
/// COM 事实），基线重验仍逐字段强核对。
pub(crate) fn validate_identity_chain(
    package_root: &str,
    host: &ProcessIdentityFact,
    renderer: Option<&ProcessIdentityFact>,
    hops: &[ParentHop],
) -> Result<IdentityBaseline, UiError> {
    let host_package = host
        .package_full_name
        .as_deref()
        .ok_or(UiError::Unavailable)?;
    if host_package.is_empty() {
        return Err(UiError::Unavailable);
    }
    require_image_inside_package(package_root, &host.image_path)?;
    match renderer {
        Some(child) => {
            let child_package = child
                .package_full_name
                .as_deref()
                .ok_or(UiError::Unavailable)?;
            if child_package.is_empty() || child_package != host_package {
                // 渲染子进程必须与宿主同包（错包拒绝）。
                return Err(UiError::Unavailable);
            }
            require_image_inside_package(package_root, &child.image_path)?;
            connect_identity_chain(child, host, hops)?;
        }
        None => {
            if !hops.is_empty() {
                // 无渲染子进程时任何父链材料都与事实不符 → 拒绝。
                return Err(UiError::Unavailable);
            }
        }
    }
    Ok(IdentityBaseline {
        host: host.clone(),
        renderer: renderer.cloned(),
        window_rid: Vec::new(),
    })
}

/// SPEC6：基线【更新判定】离线缝——生产 `read_frame` 的真实更新路径直接消费本函数
///（不再 take 后丢旧值）；离线测试可注入 previous/current 完整 FILETIME 事实与
/// `prove` 失败闭包驱动（本函数无 COM / 无存储副作用，Err 由调用方处置）。
/// 判定：
/// - `None`（仅新 observe 的新记录从此建立）→ `prove` 首次证明；
/// - `Some(previous)` 且逐字段重验通过（含完整 FILETIME）→ 保留 previous，不重证明；
/// - 唯一迁移一：卡随关闭消失（renderer Some→None 且 host/window 不变）→ current；
/// - 唯一迁移二：空回读 None 后的新合法 renderer 配对（None→Some 且 host/window
///   不变）→ `prove` 以真实证据重新证明（不继承旧 renderer/window）；
/// - 其余漂移（host/window 变化、renderer 替换、同 PID 同毫秒不同完整 FILETIME）
///   → Err。
///
/// 【状态提交合同由 `update_identity_baseline` 承担（SPEC6 窄事务缝）】：本函数
/// 只做纯判定；Err 时的「保留旧基线 + 毒化」副作用在事务缝内完成，生产
/// `read_frame` 直接调用事务缝，不自行拼装判定与提交。
pub(crate) fn settle_identity_baseline(
    previous: Option<&IdentityBaseline>,
    current: &IdentityBaseline,
    deadline: Instant,
    prove: impl FnOnce(&IdentityBaseline, Instant) -> Result<IdentityBaseline, UiError>,
) -> Result<IdentityBaseline, UiError> {
    match previous {
        None => prove(current, deadline),
        Some(previous) => {
            if previous.revalidate(current).is_ok() {
                Ok(previous.clone())
            } else {
                let renderer_released = previous.renderer.is_some() && current.renderer.is_none();
                let renderer_new_pairing =
                    previous.renderer.is_none() && current.renderer.is_some();
                let stable =
                    previous.host == current.host && previous.window_rid == current.window_rid;
                if renderer_released && stable {
                    Ok(current.clone())
                } else if renderer_new_pairing && stable {
                    prove(current, deadline)
                } else {
                    Err(UiError::Unavailable)
                }
            }
        }
    }
}

/// SPEC6 基线【真实状态提交】窄事务缝——生产 `windows.read_frame` 的 slot 提交
/// 必须直接调用本函数（不自行拼装判定/保留/毒化），离线测试以真实
/// `Option<IdentityBaseline>` slot 驱动（与生产 `ObservationRecord.identity_baseline`
/// 同一状态容器，不另存 slot 假证）。单一事务语义：
/// - Ok(基线) → slot 才被替换为该基线（唯一提交点）；
/// - Err → slot 原样保留（不 take 不清空），并执行 `poison()` 毒化本 observation
///   ——同 observation 下一次读仍拒（A→B 拒 → B 仍拒，含同毫秒不同完整
///   FILETIME）；仅新 observe 的 None slot 可重建。
///
/// 判定复用 `settle_identity_baseline`（身份规则不重写）；`poison` 由调用方注入
/// （windows 侧传 `shared.mark_poisoned` 闭合），本函数无存储假设。
pub(crate) fn update_identity_baseline(
    slot: &mut Option<IdentityBaseline>,
    current: &IdentityBaseline,
    deadline: Instant,
    prove: impl FnOnce(&IdentityBaseline, Instant) -> Result<IdentityBaseline, UiError>,
    poison: impl FnOnce(),
) -> Result<(), UiError> {
    match settle_identity_baseline(slot.as_ref(), current, deadline, prove) {
        Ok(baseline) => {
            *slot = Some(baseline);
            Ok(())
        }
        Err(err) => {
            poison();
            Err(err)
        }
    }
}

/// F8：缓存 RuntimeId 数组 VARIANT 的 exact-tag 门禁（离线纯缝；真实 union.parray
/// 与 SAFEARRAY 访问由调用方包在 `read` 内）。仅 exact `VT_ARRAY|VT_I4`（0x2003 =
/// VT_ARRAY 0x2000 | VT_I4 0x0003）允许执行 read；BYREF（VT_BYREF 0x4000）/错元素
/// 型/额外 flag/裸 VT_I4 一律拒绝——拒绝发生在访问 VARIANT union 之前，非法 tag
/// 绝不触碰 union。拒绝隐式转换，隔离错误 union 分支。
pub(crate) fn with_cached_i4_array<T>(
    vt: u16,
    read: impl FnOnce() -> Result<T, UiError>,
) -> Result<T, UiError> {
    const VT_ARRAY_FLAG: u16 = 0x2000;
    const VT_I4_TAG: u16 = 0x0003;
    if vt == VT_ARRAY_FLAG | VT_I4_TAG {
        read()
    } else {
        Err(UiError::Unavailable)
    }
}

// ---- M4：回调 RuntimeId 只读缓存 ----

/// 回调侧 RuntimeId 来源：注册含 RuntimeId 的真实 cache request 后，回调只允许读
/// 缓存 DTO；`LiveOnly` 表示缓存不可用（= Unknown/poison，禁止现场 Current/GetRuntimeId
/// 兜底）。Windows 侧 cache request 注册在【实现段】；Fake 的 live accessor 一被调
/// 即失败由测试提供。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeIdSource {
    Cached(Vec<i32>),
    /// 缓存不可用（= Unknown/poison）；仅测试直接构造该拒绝形态。
    #[cfg(test)]
    LiveOnly,
}

/// 解码缓存 RuntimeId：仅 `Cached` 且非空合法；其余（LiveOnly / 空）= Unknown → 拒绝。
pub(crate) fn decode_cached_runtime_id(source: &RuntimeIdSource) -> Result<Vec<i32>, UiError> {
    match source {
        RuntimeIdSource::Cached(rid) if !rid.is_empty() => Ok(rid.clone()),
        _ => Err(UiError::Unavailable),
    }
}

// ---- M5/M6：订阅清单（共用数据缝）----

/// 订阅种类（M5 完整覆盖面）：结构变化、属性（Name/IsOffscreen/IsEnabled/Value）、
/// Selection Invalidated、SelectionItem（selected/added/removed）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionKind {
    StructureChanged,
    PropertyChanged,
    SelectionInvalidated,
    SelectionItemEvent,
}

/// 单条已注册订阅记录（目标 = 完整 RuntimeId）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubscriptionEntry {
    pub(crate) kind: SubscriptionKind,
    pub(crate) target_rid: Vec<i32>,
}

/// 订阅清单（M6）：记录成功注册项；部分失败按逆序（后注册先撤）回滚。
/// 真实 COM unsubscribe 调用在【实现段】由 Windows adapter 依本清单执行；
/// 正常/异常退出逐项 unsubscribe 后释放 COM（M6）。Fake 断言成对计数/回滚顺序
/// 直接消费本结构。
#[derive(Default)]
pub(crate) struct SubscriptionManifest {
    committed: Vec<SubscriptionEntry>,
}

impl SubscriptionManifest {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 记录一次成功注册（顺序即注册顺序）。
    pub(crate) fn record_success(&mut self, entry: SubscriptionEntry) {
        self.committed.push(entry);
    }

    /// 部分失败的回滚计划：已成功项的逆序（后注册先撤）。
    pub(crate) fn rollback_plan(&self) -> Vec<SubscriptionEntry> {
        let mut plan = self.committed.clone();
        plan.reverse();
        plan
    }

    /// 回滚完成（全部或到某点）后清空；正常退出 unsubscribe 全部后同样清空。
    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.committed.clear();
    }

    #[cfg(test)]
    pub(crate) fn committed(&self) -> &[SubscriptionEntry] {
        &self.committed
    }
}

/// M5 订阅覆盖审计：清单必须同时覆盖结构 + 属性 + SelectionInvalidated +
/// SelectionItem 四面，缺任一 = incomplete（不冒充结构/导航全覆盖）。
pub(crate) fn audit_subscription_coverage(kinds: &[SubscriptionKind]) -> Result<(), UiError> {
    let required = [
        SubscriptionKind::StructureChanged,
        SubscriptionKind::PropertyChanged,
        SubscriptionKind::SelectionInvalidated,
        SubscriptionKind::SelectionItemEvent,
    ];
    for kind in required {
        if !kinds.contains(&kind) {
            return Err(UiError::Unavailable);
        }
    }
    Ok(())
}

// ---- M7：SAFEARRAY 长度受检计算 ----

/// SAFEARRAY 一维长度受检计算（i32 上下界 → i64 扩宽 → usize）：
/// 倒置（upper < lower）拒绝；i32 极值差在 i64 内不溢出；超过 `max_len` 拒绝；
/// 结果经 try_from 受检转换。owned 数组由调用方 RAII 销毁、借用数组仅复制
///（所有权纪律在 windows.rs，本函数只管长度数值）。
pub(crate) fn checked_array_len(lower: i32, upper: i32, max_len: usize) -> Result<usize, UiError> {
    if upper < lower {
        return Err(UiError::Unavailable);
    }
    let span = (upper as i64) - (lower as i64) + 1;
    if span <= 0 {
        return Err(UiError::Unavailable);
    }
    let length = usize::try_from(span).map_err(|_| UiError::Unavailable)?;
    if length > max_len {
        return Err(UiError::Unavailable);
    }
    Ok(length)
}

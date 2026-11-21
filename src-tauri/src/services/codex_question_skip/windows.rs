//! Windows 标准 COM / UIA 后端（真实实现，仅注册于 `cfg(windows)`）。
//!
//! **执行边界（本轮红线）**：本文件是真实 InvokePattern 后端的可编译实现，仅供
//! 后续接线（Task3 / 已认证产品命令）在未来调用；本轮绝不现场执行、不测试真 UIA、
//! 不启动应用、不代点。测试一律走 Fake（`binding_tests`），不构造真实工厂。
//!
//! 约束（与架构交接 §6 一致）：
//! - 专用无窗口 MTA 线程：`CoInitializeEx(COINIT_MULTITHREADED)`，同线程释放、
//!   `CoUninitialize`；初始化失败不发布可用 backend；
//! - 固定官方包 `OpenAI.Codex_26.1106.6628.0_x64__2p2nqsd0c76g0` +
//!   `app.asar` SHA256 `54b74a91…`（只读核对并缓存，不每 3 秒哈希 773MB）；
//!   `monitor/host.rs::is_host_process` 等宿主名识别不单独放行；
//! - 进程代际：`GetProcessTimes` 完整 creation FILETIME 私存，
//!   `WindowScope.started_at_ms` 由同源换算（不把 sysinfo 秒伪成毫秒精度）；
//!   HWND→PID 核对；渲染子进程需证明同包与所属宿主代际关系，拿不到即拒绝；
//! - 有限 TreeWalker 遍历、实际 Raw 父链、完整 RuntimeId；只接受已证活卡 Group 的
//!   直属 Dismiss（探测事实：Button + InvokePattern + 名"忽略"；跳过/发送同为直属
//!   Invoke 按钮但名称互斥）；完整题干与有序 options 比较；禁止坐标/索引/RuntimeId
//!   后缀/历史 Text 定位；不按按钮同名盲 Invoke（定位锚 = 绑定的完整 RuntimeId +
//!   直属父链复核）；
//! - 专用 StructureChanged / PropertyChanged / Selection handler，不用通用事件冒充
//!   结构事件；回调仅复制缓存身份/seq 并投递，不遍历、不等 worker、不持业务锁；
//! - SAFEARRAY 维度/类型/上下界/长度受限检查，拥有数组 RAII 销毁、回调借用数组仅
//!   复制；读超时/截断/身份不确定只能 incomplete/Err，绝不完整空；
//! - 上下文成功见证 unavailable：真实 profile 恒不签 IndependentContext，
//!   `ProviderFrame.context_witness` 恒 None（见 §3：仅卡消失必须 Unresolved）。
//!
//! 已知保守限制（如实申报，不以默认 Some(1) 或假证据凑可执行）：
//! 1. 作用域内出现多个"疑似题卡 Group"→ `capture` 直接 Err（ProviderFrame 的
//!    direct_parent/structure_digest 单值，无法确定多候选各自归属，"未知多 parent
//!    拒绝"）；多题合并面板由此无法预览，属设计内保守拒绝；
//! 2. 题干之外的有序 options 读取依据探测事实（卡 Group 直属子 Group 下的 Text），
//!    若真实形态变化/读取截断 → Err，不猜；
//! 3. 回调只携带身份与 seq，phase 由服务侧分发器标注（`EventPhase::Watching` 为
//!    占位初值）；无"独立于待删卡且导航敏感"的上下文见证实现。

#![allow(dead_code)]

use std::cell::Cell;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use sha2::{Digest, Sha256};
use windows::core::{implement, Interface, BOOL, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, APPMODEL_ERROR_NO_PACKAGE, ERROR_ACCESS_DENIED, FILETIME, HANDLE, HWND, LPARAM,
    WIN32_ERROR,
};
use windows::Win32::Storage::Packaging::Appx::{GetPackageFullName, GetPackagePathByFullName};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    SAFEARRAY,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayGetVartype,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_I4};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationEventHandler, IUIAutomationEventHandler_Impl, IUIAutomationInvokePattern,
    IUIAutomationPropertyChangedEventHandler, IUIAutomationPropertyChangedEventHandler_Impl,
    IUIAutomationStructureChangedEventHandler, IUIAutomationStructureChangedEventHandler_Impl,
    IUIAutomationTreeWalker, StructureChangeType, StructureChangeType_ChildAdded,
    StructureChangeType_ChildRemoved, StructureChangeType_ChildrenBulkAdded,
    StructureChangeType_ChildrenBulkRemoved, StructureChangeType_ChildrenInvalidated,
    StructureChangeType_ChildrenReordered, TreeScope_Subtree, UIA_ButtonControlTypeId,
    UIA_GroupControlTypeId, UIA_InvokePatternId, UIA_IsEnabledPropertyId,
    UIA_IsOffscreenPropertyId, UIA_NamePropertyId, UIA_RuntimeIdPropertyId,
    UIA_SelectionItem_ElementAddedToSelectionEventId,
    UIA_SelectionItem_ElementRemovedFromSelectionEventId, UIA_SelectionItem_ElementSelectedEventId,
    UIA_Selection_InvalidatedEventId, UIA_ValueValuePropertyId, UIA_EVENT_ID, UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindow};

use super::binding::{
    AttemptContext, CallBudget, EventKind, EventPhase, EventSink, FinalAdmission, ObservationId,
    ObservedEvent, PlatformProvider, ProviderFrame, SharedObservation,
};
use super::helper::{
    audit_budget, audit_depth, audit_siblings, audit_subscription_coverage, checked_array_len,
    decode_cached_runtime_id, update_identity_baseline, validate_identity_chain,
    with_cached_i4_array, IdentityBaseline, ParentHop, ProcessIdentityFact, RuntimeIdSource,
    SubscriptionEntry, SubscriptionKind, SubscriptionManifest,
};
use super::native_discovery::DiscoveredWindow;
use super::types::{
    InvokeTarget, NativeCard, NativeCardSnapshot, QuestionFingerprint, UiError, WindowScope,
};

// ---------------------------------------------------------------------------
// 固定身份事实（来源：已验证官方包与只读探测；不重扫、不硬编码 RuntimeId）
// ---------------------------------------------------------------------------

/// 固定官方 MSIX 包全名（PackageFullName，非 Family 名）。
const PINNED_PACKAGE_FULL_NAME: &str = "OpenAI.Codex_26.1106.6628.0_x64__2p2nqsd0c76g0";
/// 固定官方 app.asar SHA256（小写 hex）。
const PINNED_ASAR_SHA256: &str = "54b74a9131680fc9eead711ed5d9f718f6127c22222ca60d2e3b577a707e2485";
/// 探测事实（2026-11-14 result-q01-live-controls-1.json）：活卡 Group 直属关闭按钮
/// 名为"忽略"（zh），Button + InvokePattern；"跳过"/"发送"同为直属 Invoke 按钮但
/// 名称互斥。仅以【绑定完整 RuntimeId + 直属父链 + 形态 + 名称精确相等】联合认定，
/// 禁止单独按名称/索引/后缀/历史 Text 定位。
const DISMISS_BUTTON_LABEL: &str = "忽略";
/// 探测证据中的中文 UI 标签：非中文界面 → 形态不符 → 保守拒绝（UnsupportedBuild 由
/// 上层版本白名单承担，此处直接 Unavailable，不猜按钮）。
const SKIP_BUTTON_LABEL: &str = "跳过";
const SEND_BUTTON_LABEL: &str = "发送";
/// RuntimeId 长度上限（完整 RuntimeId 逐值比较，禁后缀）。
const MAX_RUNTIME_ID_LEN: usize = 64;
/// 祖先链深度上限。
const MAX_PARENT_DEPTH: usize = 48;
/// 全树遍历节点预算。
const MAX_TRAVERSAL_NODES: usize = 20_000;
/// 卡内子树 digest 节点预算。
const MAX_STRUCTURE_NODES: usize = 256;
/// 卡直属子节点上限。
const MAX_CARD_CHILDREN: usize = 64;
/// 有序 options 文本上限。
const MAX_CARD_TEXTS: usize = 32;
/// 只读发现：合格窗口上限（第 9 个合格窗口即整体失败；服务侧同约束复检）。
const MAX_DISCOVERY_WINDOWS: usize = 8;
/// 只读发现：顶层 HWND 枚举上限（第 257 个即 incomplete 失败，不返回截短集）。
const MAX_DISCOVERY_TOP_LEVEL: usize = 256;
/// 只读发现：钉定包 exe 权威集合的目录树遍历上限（超限 = 不能保证完整遍历 →
/// 不出具截短集负判；有界栈遍历，不无限跟随符号/循环结构）。
const MAX_PACKAGE_WALK_ENTRIES: usize = 16_384;
/// 文件时间 → Unix epoch ms 的 1601→1970 偏移（毫秒）。
const FILETIME_EPOCH_OFFSET_MS: u64 = 11_644_473_600_000;

// ---------------------------------------------------------------------------
// RAII 守卫
// ---------------------------------------------------------------------------

/// HANDLE RAII：CloseHandle 配对（进程句柄 / ToolHelp 快照）。
struct HandleGuard(HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 专用无窗口 MTA 初始化：`CoInitializeEx(COINIT_MULTITHREADED)` 成功即持有配对
/// `CoUninitialize`；同线程构造/释放。
struct ComApartment {
    owns_init: bool,
}

impl ComApartment {
    fn init_mta() -> Result<Self, UiError> {
        // S_OK / S_FALSE 都算成功（均需配对 CoUninitialize）；RPC_E_CHANGED_MODE 失败。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(UiError::Unavailable);
        }
        Ok(Self { owns_init: true })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owns_init {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// F1：TreeWalker 原始 ABI 的统一安全步进（唯一 unsafe 集中点）。out 先置 null；
/// 生产 closure 直接以 Interface vtable ABI 触发真实 COM 调用——不先经过高层
/// `Result` 再反推（windows-core 0.62 `Type::from_abi` 会把 S_OK+null 折成 Err，
/// 高层 Result 无法区分「合法尽头」与「真实失败」）。HRESULT 失败（含 E_POINTER）
/// → Err，不一概当末尾；成功+null → Ok(None)（合法尽头/无子，正常 leaf/兄弟末尾
/// 完整结束）；成功+非空 → 恰一次接管输出接口。真实尽头与真实失败因此可区分，
/// COM 失败/深度超限/兄弟超限/文本截断仍各自 Err，绝不静默 complete。
fn walk_step_with(
    call: impl FnOnce(*mut *mut std::ffi::c_void) -> windows::core::HRESULT,
) -> Result<Option<IUIAutomationElement>, UiError> {
    let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
    let hr = call(&mut raw);
    if hr.is_err() {
        // 真实 COM 失败：incomplete，不冒充尽头。
        return Err(UiError::Unavailable);
    }
    if raw.is_null() {
        // 成功但无输出：合法尽头（无子 / 兄弟末尾），不是错误。
        return Ok(None);
    }
    // 成功且非空：恰一次接管输出接口所有权（from_raw 不再判空/不 AddRef）。
    Ok(Some(unsafe { IUIAutomationElement::from_raw(raw) }))
}

#[cfg(test)]
mod walker_abi_contract_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn successful_null_raw_walker_output_is_a_normal_leaf() {
        let called = Cell::new(false);
        let result = walk_step_with(|out| {
            called.set(true);
            assert!(unsafe { (*out).is_null() });
            windows::core::HRESULT(0)
        });
        assert!(called.get(), "raw ABI must be invoked");
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn failed_raw_walker_call_is_not_a_normal_leaf() {
        let called = Cell::new(false);
        let result = walk_step_with(|out| {
            called.set(true);
            assert!(unsafe { (*out).is_null() });
            windows::core::HRESULT(0x80004005_u32 as i32)
        });
        assert!(called.get(), "raw ABI must be invoked");
        assert!(result.is_err());
    }
}

#[cfg(test)]
#[path = "windows_discovery_seam_tests.rs"]
mod discovery_seam_tests;

/// SAFEARRAY RAII：仅用于 GetRuntimeId 等拥有的数组；回调借用数组不得销毁。
struct SafeArrayGuard(*mut SAFEARRAY);
impl SafeArrayGuard {
    fn new(array: *mut SAFEARRAY) -> Result<Self, UiError> {
        if array.is_null() {
            return Err(UiError::Unavailable);
        }
        Ok(Self(array))
    }
}
impl Drop for SafeArrayGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = SafeArrayDestroy(self.0);
            }
        }
    }
}

/// VARIANT RAII：外层 VariantClear 配对（缓存 RuntimeId 数组的所有权归 VARIANT，
/// 借用读取仅复制绝不 SafeArrayDestroy，读后由本守卫统一释放；任何退出路径都清理）。
struct VariantGuard(*mut VARIANT);
impl Drop for VariantGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// 进程身份与代际
// ---------------------------------------------------------------------------

/// 进程代际戳：`GetProcessTimes` 的完整 creation FILETIME（100ns 刻度）私存，
/// `started_at_ms` 由同源换算，不借用 sysinfo 秒精度。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessStamp {
    creation_filetime: u64,
}

impl ProcessStamp {
    /// FILETIME(100ns, 1601) → Unix epoch ms（同源换算，非伪造精度）。
    fn started_at_ms(&self) -> u64 {
        self.creation_filetime
            .checked_div(10_000)
            .unwrap_or(0)
            .saturating_sub(FILETIME_EPOCH_OFFSET_MS)
    }
}

/// 进程只读身份：包全名（未打包为 None）、镜像路径、代际戳。
#[derive(Clone, Debug)]
pub(crate) struct ProcessIdentity {
    pid: u32,
    package_full_name: Option<String>,
    image_path: String,
    stamp: ProcessStamp,
}

// ---------------------------------------------------------------------------
// M3 review seam（review 9e11-M3）：顶层枚举收集器提取（纯夹具可测，无行为
// 变更——discover_windows 引用同一实现）+ 包查询错误分类与候选步骤编排。
// GREEN：分类/编排为生产真实实现，discover_windows 生产路径调用**同一**
// `evaluate_candidate_steps` / `collect_top_level`，不留镜像副本。
// ---------------------------------------------------------------------------

/// 顶层窗口收集器（原 discover_windows 内嵌项，提取为模块级供纯夹具测试；
/// 行为不变：仅复制 HWND，第 257 个顶层窗口主动停 → truncated，不保存第 257 个）。
struct TopLevelWindows {
    hwnds: Vec<isize>,
    truncated: bool,
}

/// EnumWindows 回调：不触碰 COM/UIA；第 257 个即 truncated 整体失败（无 partial）。
unsafe extern "system" fn collect_top_level(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let collected = &mut *(lparam.0 as *mut TopLevelWindows);
    if collected.hwnds.len() >= MAX_DISCOVERY_TOP_LEVEL {
        collected.truncated = true;
        return BOOL(0);
    }
    collected.hwnds.push(hwnd.0 as isize);
    BOOL(1)
}

/// 包查询/进程打开的结果分类输入（真实错误面：read_package_full_name 的
/// NO_PACKAGE/122/其余三态 + OpenProcess 的 ACCESS_DENIED 分类）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PackageQueryOutcome {
    /// 包全名读取成功（首段缓冲协商 + 第二段查询成功）。
    Named(String),
    /// APPMODEL_ERROR_NO_PACKAGE：已证未打包进程（唯一可安全排除的无包证据）。
    ProvenUnpackaged,
    /// OpenProcess 被拒：无独立权威无关证据时不可分类。
    ProcessDenied,
    /// 其余错误（句柄失效/未知错误码）：未知一律不可当无关。
    UnknownError,
}

/// 候选窗口裁决。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CandidateVerdict {
    /// 包名命中：继续 ASAR/元素/代际核对。
    Proceed,
    /// 已证与本应用无关：早排除，不得再调用后续读取步骤。
    ExcludeUnrelated,
    /// 不可分类：整体失败（不冒充无关、不返回截短候选集）。
    FailWhole,
}

/// 候选窗口步骤编排（生产实现，seam 测试与 discover_windows 共用同一函数）：
/// 返回 (裁决, image 步骤实际调用次数, creation-time 步骤实际调用次数)。
/// - 已证无关（未打包/外来包名/被拒+独立证据）在后续读取之前排除（调用次数 0）；
/// - 未知错误与无证据被拒必须整体失败（不冒充无关、不返回截短候选集）；
/// - 钉定包命中 → 真实执行 image/creation-time 读取（fail-closed，失败整体失败）
///   后 Proceed。
pub(crate) fn evaluate_candidate_steps(
    pinned_package: &str,
    independent_unrelated_evidence: bool,
    package: &mut dyn FnMut() -> PackageQueryOutcome,
    image: &mut dyn FnMut() -> Result<String, UiError>,
    creation_time: &mut dyn FnMut() -> Result<u64, UiError>,
) -> (CandidateVerdict, usize, usize) {
    match package() {
        // 已证未打包：与本应用无关，早排除。
        PackageQueryOutcome::ProvenUnpackaged => (CandidateVerdict::ExcludeUnrelated, 0, 0),
        // 外来包名（非钉定包）：已证无关，早排除。
        PackageQueryOutcome::Named(name) if name != pinned_package => {
            (CandidateVerdict::ExcludeUnrelated, 0, 0)
        }
        // 钉定包命中：真实执行后续读取步骤（fail-closed），全过才 Proceed。
        PackageQueryOutcome::Named(_) => {
            let mut image_used = 0usize;
            let mut time_used = 0usize;
            image_used += 1;
            if image().is_err() {
                return (CandidateVerdict::FailWhole, image_used, time_used);
            }
            time_used += 1;
            if creation_time().is_err() {
                return (CandidateVerdict::FailWhole, image_used, time_used);
            }
            (CandidateVerdict::Proceed, image_used, time_used)
        }
        // OpenProcess 被拒：仅独立权威无关证据可排除；否则整体失败。
        PackageQueryOutcome::ProcessDenied if independent_unrelated_evidence => {
            (CandidateVerdict::ExcludeUnrelated, 0, 0)
        }
        // 未知错误与无证据被拒：不可分类 → 整体失败。
        PackageQueryOutcome::ProcessDenied | PackageQueryOutcome::UnknownError => {
            (CandidateVerdict::FailWhole, 0, 0)
        }
    }
}

/// 已打开句柄上的包全名三态分类（M3 严格面，交由 evaluate_candidate_steps 编排）。
fn query_package_outcome(handle: HANDLE) -> PackageQueryOutcome {
    match read_package_full_name(handle) {
        Ok(Some(name)) => PackageQueryOutcome::Named(name),
        Ok(None) => PackageQueryOutcome::ProvenUnpackaged,
        Err(_) => PackageQueryOutcome::UnknownError,
    }
}

/// OpenProcess 失败分类：错误码为 HRESULT——ACCESS_DENIED 的等价正确码 =
/// `HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED)`（0x80070005），经
/// `HRESULT::from_win32` 精确构造（不得与裸 WIN32_ERROR(5) 数值比较）→
/// ProcessDenied（仅独立权威无关证据可排除）；其余 → UnknownError（整体失败）。
fn open_process_outcome(error: &windows::core::Error) -> PackageQueryOutcome {
    if error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
        PackageQueryOutcome::ProcessDenied
    } else {
        PackageQueryOutcome::UnknownError
    }
}

/// 钉定包允许 exe 名集合（M3 权威范围，**完整递归遍历**）：固定包安装目录
/// （`package_path_of` 同源 = `verify_pinned_identity` 的钉定路径事实）整棵目录
/// 树下的 *.exe 文件名（小写）。完整性纪律：任何目录/条目/名称读取失败、
/// 超预算、超遍历上限 = 不能保证完整遍历 → None（**不出具截短集负判**；
/// 禁止 flatten 吞错）；有界栈遍历，不跟随循环无界。空集 → None（含糊）。
fn pinned_package_exe_allowlist(deadline: Instant) -> Option<Vec<String>> {
    let package_path = package_path_of(PINNED_PACKAGE_FULL_NAME).ok()?;
    let mut stack = vec![std::path::PathBuf::from(package_path)];
    let mut names = Vec::new();
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        if Instant::now() >= deadline {
            // 超预算：不能安全保证完整遍历 → 不出具截短集负判。
            return None;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // 任一目录读取失败 = 不能完整遍历 → 无负证据。
            Err(_) => return None,
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                // 单项枚举失败 = 不能完整遍历 → 无负证据（禁止吞错）。
                Err(_) => return None,
            };
            visited += 1;
            if visited > MAX_PACKAGE_WALK_ENTRIES {
                // 遍历上限：不能安全保证完整 → 不出具截短集负判（有界，不无限跟随）。
                return None;
            }
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                // 条目属性不可读 = 不能完整分类 → 无负证据。
                Err(_) => return None,
            };
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            let is_exe = path
                .extension()
                .map(|extension| extension.eq_ignore_ascii_case("exe"))
                .unwrap_or(false);
            if !is_exe {
                continue;
            }
            // exe 名损坏（非 UTF-8）→ 权威范围不可信：不出具证据。
            let name = path.file_name().and_then(|name| name.to_str())?;
            names.push(name.to_lowercase());
        }
    }
    if names.is_empty() {
        // 权威范围可得却零 exe：含糊 → 不出具证据（保守 FailWhole）。
        return None;
    }
    Some(names)
}

/// OpenProcess 被拒 pid 的独立权威无关证据（M3 严格负向，与固定 Native 允许
/// exe 路径范围一致）：Kernel ToolHelp 进程表实读被拒 pid 的可执行文件名，比对
/// 钉定包安装目录**完整递归** *.exe 权威集合（同 `verify_pinned_identity` 钉定
/// 路径事实；遍历不完整/超预算一律不出证据，绝不拿截短集负判）——名字**不在
/// 集合内** = 已证无关（负向证据，永不正向准入）。快照失败 / 表内无该 pid /
/// 名字为空或损坏（含 lossy 替换符）→ 无证据（保守 FailWhole，不硬填）。固定
/// MSIX/ASAR/完整 filetime/parent/RID 最终强制不降。
fn toolhelp_unrelated_evidence(pid: u32, deadline: Instant) -> bool {
    let Some(allowlist) = pinned_package_exe_allowlist(deadline) else {
        return false;
    };
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(snapshot) => snapshot,
        // 快照失败 = 无证据（保守 FailWhole），不假装无关。
        Err(_) => return false,
    };
    let _guard = HandleGuard(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_err() {
            return false;
        }
        loop {
            if entry.th32ProcessID == pid {
                let text_end = entry
                    .szExeFile
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(entry.szExeFile.len());
                if text_end == 0 {
                    return false; // 空名字 = 未知，不出证据。
                }
                let exe = String::from_utf16_lossy(&entry.szExeFile[..text_end]);
                if exe.contains('\u{FFFD}') {
                    return false; // 损坏 lossy 名 = 未知，不出证据。
                }
                let exe_lower = exe.to_lowercase();
                return !allowlist.contains(&exe_lower);
            }
            if Process32NextW(snapshot, &mut entry).is_err() {
                return false; // 表内无该 pid = 无证据（保守 FailWhole）。
            }
        }
    }
}

/// FILETIME(100ns, 1601) → Unix epoch ms（同源换算，非伪造精度）。
fn filetime_to_unix_ms(filetime: u64) -> u64 {
    filetime
        .checked_div(10_000)
        .unwrap_or(0)
        .saturating_sub(FILETIME_EPOCH_OFFSET_MS)
}

/// 两段式读取包全名（M3 严格三态）：首段仅 APPMODEL_ERROR_NO_PACKAGE 可判已证
/// 未打包（Ok(None)）；ERROR_INSUFFICIENT_BUFFER 走缓冲协商（第二段成功 → Some、
/// 失败 → Err）；其余首段错误 → Err（未知，不可吞成 None/无关——不得冒充无包）。
fn read_package_full_name(handle: HANDLE) -> Result<Option<String>, UiError> {
    let mut length: u32 = 0;
    let first = unsafe { GetPackageFullName(handle, &mut length, None) };
    if first == APPMODEL_ERROR_NO_PACKAGE {
        return Ok(None);
    }
    if first != WIN32_ERROR(122) {
        // 非 NO_PACKAGE 且非 122：句柄失效/未知错误码 = 未知，整体失败（fail-closed）。
        return Err(UiError::Unavailable);
    }
    if length == 0 || length as usize > 4096 {
        return Err(UiError::Unavailable);
    }
    let mut buffer = vec![0u16; length as usize];
    let second =
        unsafe { GetPackageFullName(handle, &mut length, Some(PWSTR(buffer.as_mut_ptr()))) };
    if second.0 != 0 {
        return Err(UiError::Unavailable);
    }
    let text =
        unsafe { PWSTR(buffer.as_mut_ptr()).to_string() }.map_err(|_| UiError::Unavailable)?;
    Ok(Some(text))
}

fn read_process_image_path(handle: HANDLE) -> Result<String, UiError> {
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(|_| UiError::Unavailable)?;
    let text =
        unsafe { PWSTR(buffer.as_mut_ptr()).to_string() }.map_err(|_| UiError::Unavailable)?;
    Ok(text)
}

fn read_process_stamp(handle: HANDLE) -> Result<ProcessStamp, UiError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|_| UiError::Unavailable)?;
    let filetime = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
    Ok(ProcessStamp {
        creation_filetime: filetime,
    })
}

/// 只读进程身份（OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)）。
fn read_process_identity(pid: u32) -> Result<ProcessIdentity, UiError> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|_| UiError::Unavailable)?;
    let _guard = HandleGuard(handle);
    Ok(ProcessIdentity {
        pid,
        package_full_name: read_package_full_name(handle)?,
        image_path: read_process_image_path(handle)?,
        stamp: read_process_stamp(handle)?,
    })
}

/// ToolHelp 父 PID 快照（子 renderer 的宿主代际证明材料之一）。
fn parent_pid_of(pid: u32) -> Result<Option<u32>, UiError> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|_| UiError::Unavailable)?;
    let _guard = HandleGuard(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_err() {
            return Ok(None);
        }
        loop {
            if entry.th32ProcessID == pid {
                return Ok(Some(entry.th32ParentProcessID));
            }
            if Process32NextW(snapshot, &mut entry).is_err() {
                return Ok(None);
            }
        }
    }
}

/// pid → 完整 creation FILETIME（OpenProcess + GetProcessTimes，100ns 刻度完整值）。
fn read_process_filetime(pid: u32) -> Result<u64, UiError> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|_| UiError::Unavailable)?;
    let _guard = HandleGuard(handle);
    Ok(read_process_stamp(handle)?.creation_filetime)
}

/// F7：宿主代际父链的有界真实取材——每条 edge `child(pid, 完整 creation FILETIME)
/// → parent(pid, 完整 creation FILETIME)`，沿 ToolHelp 父 PID 逐跳 OpenProcess 实读
/// 代际（不能只数字 PPID：PID 可复用，代际必须由完整 FILETIME 证明）。到达 host 后
/// 前后核关键 stamps（重读 child/host 代际；取材期间死亡/重启替换 → 漂移拒绝）。
/// child 即 host → 空 hops；找不到 host 祖先 / 快照失败 / 超预算 / 超时 → Err。
fn collect_parent_hops(
    child_pid: u32,
    host_pid: u32,
    deadline: Instant,
) -> Result<Vec<ParentHop>, UiError> {
    if child_pid == host_pid {
        return Ok(Vec::new());
    }
    let child_filetime = read_process_filetime(child_pid)?;
    let mut hops = Vec::new();
    let mut current_pid = child_pid;
    let mut current_filetime = child_filetime;
    for _ in 0..MAX_PARENT_DEPTH {
        if Instant::now() >= deadline {
            return Err(UiError::Timeout);
        }
        let parent_pid = parent_pid_of(current_pid)?.ok_or(UiError::Unavailable)?;
        if parent_pid == 0 || parent_pid == current_pid {
            return Err(UiError::Unavailable);
        }
        let parent_filetime = read_process_filetime(parent_pid)?;
        hops.push(ParentHop {
            child_pid: current_pid,
            child_filetime: current_filetime,
            parent_pid,
            parent_filetime,
        });
        if parent_pid == host_pid {
            // 前后核：重读两端关键代际，取材期间漂移 = 保守拒绝。
            if read_process_filetime(child_pid)? != child_filetime
                || read_process_filetime(host_pid)? != parent_filetime
            {
                return Err(UiError::Unavailable);
            }
            return Ok(hops);
        }
        current_pid = parent_pid;
        current_filetime = parent_filetime;
    }
    Err(UiError::Unavailable)
}

/// 固定包 + ASAR 的只读核对（缓存结论；不每 3 秒哈希 773MB）。
/// 缓存键 = (包路径, asar 身份 (len, modified))；身份变化才重新哈希核对。
static ASAR_VERIFICATION: OnceLock<Mutex<Option<AsarCache>>> = OnceLock::new();

#[derive(Clone, Debug)]
struct AsarCache {
    package_path: String,
    asar_identity: (u64, SystemTime),
    asar_relative_path: &'static str,
    matches_pinned: bool,
}

/// 已知 MSIX 布局候选（只读枚举；身份由固定 SHA256 钉定，路径仅是取材位置）。
const ASAR_CANDIDATE_PATHS: &[&str] = &["app.asar", "app\\resources\\app.asar"];

fn package_path_of(full_name: &str) -> Result<String, UiError> {
    let wide: windows::core::HSTRING = full_name.into();
    let mut length: u32 = 0;
    let first = unsafe { GetPackagePathByFullName(PCWSTR(wide.as_ptr()), &mut length, None) };
    if first.0 != 122 || length == 0 || length as usize > 4096 {
        return Err(UiError::Unavailable);
    }
    let mut buffer = vec![0u16; length as usize];
    let second = unsafe {
        GetPackagePathByFullName(
            PCWSTR(wide.as_ptr()),
            &mut length,
            Some(PWSTR(buffer.as_mut_ptr())),
        )
    };
    if second.0 != 0 {
        return Err(UiError::Unavailable);
    }
    unsafe { PWSTR(buffer.as_mut_ptr()).to_string() }.map_err(|_| UiError::Unavailable)
}

/// 固定包全名比对 + 包内 app.asar SHA256 只读核对（带缓存）。
/// 候选布局逐一只读哈希，命中固定 SHA256 者即官方 asar；全不命中拒绝。
fn verify_pinned_identity(identity: &ProcessIdentity, deadline: Instant) -> Result<(), UiError> {
    let package = identity
        .package_full_name
        .as_deref()
        .unwrap_or("")
        .to_string();
    if package != PINNED_PACKAGE_FULL_NAME {
        // 宿主名/同名进程/未打包进程一律不放行。
        return Err(UiError::Unavailable);
    }
    let path = package_path_of(&package)?;
    let lock = ASAR_VERIFICATION.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().map_err(|_| UiError::Unavailable)?;
    if let Some(cache) = guard.as_ref() {
        if cache.package_path == path && cache.matches_pinned {
            let candidate = std::path::Path::new(&path).join(cache.asar_relative_path);
            if let Ok(metadata) = std::fs::metadata(&candidate) {
                let modified = metadata.modified().map_err(|_| UiError::Unavailable)?;
                if cache.asar_identity == (metadata.len(), modified) {
                    // 缓存命中且文件身份未变：直接采用既有结论。
                    return Ok(());
                }
            }
        }
    }
    // 缓存未命中 / 包路径变化 / 文件身份变化：重新只读核对（预算超限保守失败）。
    let mut resolved: Option<AsarCache> = None;
    for relative in ASAR_CANDIDATE_PATHS {
        let candidate = std::path::Path::new(&path).join(relative);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        let modified = metadata.modified().map_err(|_| UiError::Unavailable)?;
        if Instant::now() >= deadline {
            return Err(UiError::Timeout);
        }
        let Ok(bytes) = std::fs::read(&candidate) else {
            continue;
        };
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        resolved = Some(AsarCache {
            package_path: path.clone(),
            asar_identity: (metadata.len(), modified),
            asar_relative_path: relative,
            matches_pinned: hex == PINNED_ASAR_SHA256,
        });
        if resolved.as_ref().map(|cache| cache.matches_pinned) == Some(true) {
            break;
        }
    }
    let cache = resolved.ok_or(UiError::Unavailable)?;
    let matches = cache.matches_pinned;
    *guard = Some(cache);
    if matches {
        Ok(())
    } else {
        Err(UiError::Unavailable)
    }
}

/// 窗口/元素进程身份放行：与窗口宿主同 PID 直接放行；渲染子进程必须证明
/// 同固定包 + 所属宿主代际（真实 FILETIME 父链，逐边可连到宿主原代际）；
/// 宿主名不单独放行。
fn verify_element_process(
    element_pid: u32,
    host_identity: &ProcessIdentity,
    deadline: Instant,
) -> Result<ProcessStamp, UiError> {
    if element_pid == host_identity.pid {
        return Ok(host_identity.stamp);
    }
    let child = read_process_identity(element_pid)?;
    let child_package = child.package_full_name.as_deref().unwrap_or("");
    if child_package != PINNED_PACKAGE_FULL_NAME {
        return Err(UiError::Unavailable);
    }
    // F7：代际证明用完整 FILETIME 父链（不是数字 PPID 近似）。
    collect_parent_hops(element_pid, host_identity.pid, deadline)?;
    Ok(child.stamp)
}

/// F7：以真实证据证明当前身份并产出基线（基线建立/新 renderer 配对时调用，
/// 不以后续相等比较替代初次可信校验）——固定官方包根（pinned 包全名 → 实际包
/// 路径）+ host/renderer 规范 imagePath 目录组件校验 + renderer→host 完整
/// FILETIME 代际链（有界取材 + 前后核 stamps）。window RID 由本 adapter 以实读
/// 完整 RuntimeId 填充（helper 离线缝无 COM 事实），基线重验仍逐字段强核对。
fn proven_identity_baseline(
    current: &IdentityBaseline,
    deadline: Instant,
) -> Result<IdentityBaseline, UiError> {
    let package_root = package_path_of(PINNED_PACKAGE_FULL_NAME)?;
    let hops = match current.renderer.as_ref() {
        Some(child) => collect_parent_hops(child.pid, current.host.pid, deadline)?,
        None => Vec::new(),
    };
    let mut baseline = validate_identity_chain(
        &package_root,
        &current.host,
        current.renderer.as_ref(),
        &hops,
    )?;
    baseline.window_rid = current.window_rid.clone();
    Ok(baseline)
}

// ---------------------------------------------------------------------------
// RuntimeId / 属性读取（SAFEARRAY 受限解析）
// ---------------------------------------------------------------------------

/// 从 SAFEARRAY 复制完整 RuntimeId（VT_I4 一维）。
/// `owned=false`：回调借用数组——仅复制，绝不销毁（所有权归 UIA）。
fn read_runtime_id(array: *mut SAFEARRAY, owned: bool) -> Result<Vec<i32>, UiError> {
    // 借用/拥有两种来源都在此统一为裸指针；所有权差异只影响下方 guard。
    let pointer = array;
    if pointer.is_null() {
        return Err(UiError::Unavailable);
    }
    let guard = if owned {
        Some(SafeArrayGuard::new(pointer)?)
    } else {
        None
    };
    let result = read_runtime_id_inner(pointer);
    drop(guard);
    result
}

fn read_runtime_id_inner(array: *const SAFEARRAY) -> Result<Vec<i32>, UiError> {
    let dims = unsafe { SafeArrayGetDim(array) };
    if dims != 1 {
        // 维度必须为 1；多维度一律拒绝（受限解析）。
        return Err(UiError::Unavailable);
    }
    let vartype = unsafe { SafeArrayGetVartype(array) }.map_err(|_| UiError::Unavailable)?;
    if vartype != VT_I4 {
        return Err(UiError::Unavailable);
    }
    let lower = unsafe { SafeArrayGetLBound(array, 1) }.map_err(|_| UiError::Unavailable)?;
    let upper = unsafe { SafeArrayGetUBound(array, 1) }.map_err(|_| UiError::Unavailable)?;
    // M7：i32 上下界 → i64 扩宽受检长度（倒置/极值差/超限/空一律拒绝，不截断）。
    let length = checked_array_len(lower, upper, MAX_RUNTIME_ID_LEN)?;
    let mut values = Vec::with_capacity(length);
    for offset in 0..length as i32 {
        let index = lower + offset;
        let mut value: i32 = 0;
        unsafe {
            SafeArrayGetElement(
                array,
                &index,
                &mut value as *mut i32 as *mut core::ffi::c_void,
            )
        }
        .map_err(|_| UiError::Unavailable)?;
        values.push(value);
    }
    Ok(values)
}

/// M4：从缓存 VARIANT 解码 RuntimeId（exact `VT_ARRAY|VT_I4` SAFEARRAY）。
/// F8：完整 tag 门禁在 union 访问之前——BYREF/错元素型/额外 flag 在 helper 的
/// `with_cached_i4_array` 处拒绝且绝不触碰 union；union.parray 与 SAFEARRAY 读取
/// 全部包在 read closure 内（invalid tag 不先取指针）。VARIANT 外层 RAII
/// VariantClear（任何退出路径都释放）；借用数组仅复制不销毁，owned GetRuntimeId
/// 数组才由 SafeArrayGuard 销毁（见 read_runtime_id）。失败 = Unknown，
/// 绝不回退现场 GetRuntimeId 读（回调只用 cache）。
fn decode_cached_runtime_id_variant(variant: &mut VARIANT) -> Result<Vec<i32>, UiError> {
    let _clear = VariantGuard(variant as *mut VARIANT);
    let vt = unsafe { variant.Anonymous.Anonymous.vt };
    let decoded = with_cached_i4_array(vt.0, || {
        let array = unsafe { variant.Anonymous.Anonymous.Anonymous.parray };
        read_runtime_id(array, false)
    })?;
    // 统一经 helper 的缓存来源判定（空/非法 = Unknown）。
    decode_cached_runtime_id(&RuntimeIdSource::Cached(decoded))
}

/// M4：回调侧 sender 身份只读缓存（cache request 已含 RuntimeId）；
/// 缓存缺失/失败 = Unknown，禁 Current/GetRuntimeId 现场补读。
fn element_cached_runtime_id(element: &IUIAutomationElement) -> Result<Vec<i32>, UiError> {
    let mut variant = unsafe { element.GetCachedPropertyValue(UIA_RuntimeIdPropertyId) }
        .map_err(|_| UiError::Unavailable)?;
    decode_cached_runtime_id_variant(&mut variant)
}

/// 元素完整 RuntimeId（拥有数组，RAII 销毁）。
fn element_runtime_id(element: &IUIAutomationElement) -> Result<Vec<i32>, UiError> {
    let array = unsafe { element.GetRuntimeId() }.map_err(|_| UiError::Unavailable)?;
    read_runtime_id(array, true)
}

fn element_name(element: &IUIAutomationElement) -> Result<String, UiError> {
    let name = unsafe { element.CurrentName() }.map_err(|_| UiError::Unavailable)?;
    Ok(name.to_string())
}

fn element_control_type(element: &IUIAutomationElement) -> Result<i32, UiError> {
    let control_type = unsafe { element.CurrentControlType() }.map_err(|_| UiError::Unavailable)?;
    Ok(control_type.0)
}

fn element_process_id(element: &IUIAutomationElement) -> Result<u32, UiError> {
    let pid = unsafe { element.CurrentProcessId() }.map_err(|_| UiError::Unavailable)?;
    Ok(pid.max(0) as u32)
}

fn element_has_invoke_pattern(element: &IUIAutomationElement) -> bool {
    unsafe { element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) }
        .is_ok()
}

fn element_invoke_pattern(
    element: &IUIAutomationElement,
) -> Result<IUIAutomationInvokePattern, UiError> {
    unsafe { element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) }
        .map_err(|_| UiError::Unavailable)
}

fn element_control_type_name(element: &IUIAutomationElement) -> Result<String, UiError> {
    let control_type = element_control_type(element)?;
    if control_type == UIA_ButtonControlTypeId.0 {
        Ok("Button".into())
    } else if control_type == UIA_GroupControlTypeId.0 {
        Ok("Group".into())
    } else {
        Ok(format!("ControlType({control_type})"))
    }
}

// ---------------------------------------------------------------------------
// 回调转发 handler（M4/M6：缓存解码前计入共享 in-flight；seq/phase 由 ingress 就位）
// ---------------------------------------------------------------------------

/// StructureChanged 专用转发（不用通用 event 冒充结构事件）。
/// 顺序：共享 begin_callback（in-flight +1，观察已卸载即静默丢弃）→ 缓存解码身份
/// → 组装事件（seq/phase 占位，由 ingress 登记就位）→ sink 投递 → end_callback。
/// 借用 runtimeid 数组仅复制，绝不销毁；缓存失败 → Unknown（不 live 补读）。
#[implement(IUIAutomationStructureChangedEventHandler)]
struct StructureForwarder {
    shared: Arc<SharedObservation>,
    sink: EventSink,
}

impl IUIAutomationStructureChangedEventHandler_Impl for StructureForwarder_Impl {
    fn HandleStructureChangedEvent(
        &self,
        sender: windows::core::Ref<IUIAutomationElement>,
        changetype: StructureChangeType,
        runtimeid: *const SAFEARRAY,
    ) -> windows::core::Result<()> {
        // 只复制缓存身份并投递；不遍历、不等 worker、不操作 DB/业务锁。
        let proceed = match self.shared.begin_callback() {
            Ok(proceed) => proceed,
            Err(_) => return Ok(()),
        };
        if !proceed {
            // M6：alive=false（观察已卸载）后不投递。
            return Ok(());
        }
        let observation = self
            .shared
            .observation()
            .ok()
            .flatten()
            .unwrap_or(ObservationId(0));
        let sender_id = sender
            .ok()
            .ok()
            .and_then(|element| element_cached_runtime_id(element).ok());
        // 常量比较（不做 pattern 匹配，避免大小写 lint；顺序敏感：先精确后兜底）。
        let kind = if changetype == StructureChangeType_ChildRemoved {
            EventKind::ChildRemoved
        } else if changetype == StructureChangeType_ChildAdded {
            EventKind::ChildAdded
        } else if changetype == StructureChangeType_ChildrenBulkAdded
            || changetype == StructureChangeType_ChildrenBulkRemoved
            || changetype == StructureChangeType_ChildrenInvalidated
            || changetype == StructureChangeType_ChildrenReordered
        {
            EventKind::Bulk
        } else {
            EventKind::Unknown
        };
        let removed = if kind == EventKind::ChildRemoved {
            read_runtime_id(runtimeid as *mut SAFEARRAY, false).ok()
        } else {
            None
        };
        (self.sink)(ObservedEvent {
            observation,
            seq: 0,
            phase: EventPhase::Watching,
            kind,
            sender: sender_id,
            removed,
        });
        self.shared.end_callback();
        Ok(())
    }
}

/// PropertyChanged + Selection 专用转发（一个 COM 对象双接口）。
/// PropertyChanged 不深读 newvalue（DTO 不携带值），仅登记缓存身份。
#[implement(IUIAutomationPropertyChangedEventHandler, IUIAutomationEventHandler)]
struct PropertySelectionForwarder {
    shared: Arc<SharedObservation>,
    sink: EventSink,
}

impl PropertySelectionForwarder {
    fn forward(&self, sender: windows::core::Ref<IUIAutomationElement>, kind: EventKind) {
        let proceed = match self.shared.begin_callback() {
            Ok(proceed) => proceed,
            Err(_) => return,
        };
        if !proceed {
            return;
        }
        let observation = self
            .shared
            .observation()
            .ok()
            .flatten()
            .unwrap_or(ObservationId(0));
        let sender_id = sender
            .ok()
            .ok()
            .and_then(|element| element_cached_runtime_id(element).ok());
        (self.sink)(ObservedEvent {
            observation,
            seq: 0,
            phase: EventPhase::Watching,
            kind,
            sender: sender_id,
            removed: None,
        });
        self.shared.end_callback();
    }
}

impl IUIAutomationPropertyChangedEventHandler_Impl for PropertySelectionForwarder_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: windows::core::Ref<IUIAutomationElement>,
        _propertyid: UIA_PROPERTY_ID,
        _newvalue: &VARIANT,
    ) -> windows::core::Result<()> {
        self.forward(sender, EventKind::PropertyChanged);
        Ok(())
    }
}

impl IUIAutomationEventHandler_Impl for PropertySelectionForwarder_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: windows::core::Ref<IUIAutomationElement>,
        _eventid: windows::Win32::UI::Accessibility::UIA_EVENT_ID,
    ) -> windows::core::Result<()> {
        self.forward(sender, EventKind::SelectionChanged);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 遍历预算与卡扫描
// ---------------------------------------------------------------------------

/// 有界遍历：节点预算 + 同一绝对 deadline；超限 → Err（incomplete，绝不完整空）。
struct TraversalBudget {
    remaining: Cell<usize>,
    deadline: Instant,
}

impl TraversalBudget {
    fn new(max_nodes: usize, deadline: Instant) -> Self {
        Self {
            remaining: Cell::new(max_nodes),
            deadline,
        }
    }

    fn visit(&self) -> Result<(), UiError> {
        if Instant::now() >= self.deadline {
            return Err(UiError::Timeout);
        }
        // M2 预算审计与离线 helper 同一判定：预算耗尽 → incomplete（不静默完成）。
        let remaining = self.remaining.get();
        audit_budget(remaining)?;
        self.remaining.set(remaining - 1);
        Ok(())
    }
}

/// 卡扫描产物：候选卡（≤1）、候选父链（window → …→ 直父，不含卡）、单值归属、
/// 候选卡进程 PID（M3 renderer 身份事实来源）。
struct CardScan {
    cards: Vec<NativeCard>,
    parent_chain: Vec<Vec<i32>>,
    direct_parent: Vec<i32>,
    structure_digest: String,
    complete: bool,
    card_pid: Option<u32>,
}

// ---------------------------------------------------------------------------
// WindowsProvider
// ---------------------------------------------------------------------------

/// 一次订阅的登记：观察/共享状态、窗口元素与句柄、订阅清单与事件 ID（逆序移除用）。
struct ObservationRecord {
    observation: ObservationId,
    shared: Arc<SharedObservation>,
    window: IUIAutomationElement,
    window_rid: Vec<i32>,
    /// F7：已证身份基线归本 observation——unobserve 随记录丢弃，新 observe 从
    /// None 重建（不永久继承旧 renderer/window；空回读 None 后的新合法配对经
    /// `proven_identity_baseline` 重新证明）。
    identity_baseline: Option<IdentityBaseline>,
    /// M6 清单：已成功注册项（逆序回滚计划来源）。
    manifest: SubscriptionManifest,
    /// 已注册的自动化事件 ID（注册顺序；移除时逆序弹出）。
    automation_events: Vec<UIA_EVENT_ID>,
    structure: IUIAutomationStructureChangedEventHandler,
    property: IUIAutomationPropertyChangedEventHandler,
    selection: IUIAutomationEventHandler,
    cache_request: IUIAutomationCacheRequest,
}

/// candidate provider 的 seed 身份（来自 `DiscoveredWindow`，由
/// `windows_provider_factory_for` 注入；Observe 前与逐读复核）。
/// 代际证据是**完整 100ns creation FILETIME**（非 ms 近似；同毫秒不同 FILETIME
/// 代表不同进程实例，必须拒绝）。
#[derive(Clone, Debug)]
struct SeedIdentity {
    hwnd: isize,
    pid: u32,
    creation_filetime: u64,
    window_rid: Vec<i32>,
}

/// 候选卡识别结果：(卡元素, 卡完整 RuntimeId, dismiss 元素, dismiss 完整 RuntimeId)。
type CardCandidate = (
    IUIAutomationElement,
    Vec<i32>,
    IUIAutomationElement,
    Vec<i32>,
);

/// Windows 真实 COM/UIA 后端。由 `windows_provider_factory` /
/// `windows_provider_factory_for` 在 MTA worker 线程内创建；COM 对象不跨线程。
/// 仅后续接线（Task3 / 已认证命令）调用，本轮不执行。
pub(crate) struct WindowsProvider {
    automation: IUIAutomation,
    raw_walker: IUIAutomationTreeWalker,
    control_walker: IUIAutomationTreeWalker,
    observation: Option<ObservationRecord>,
    next_observation_seq: u64,
    /// Observe 前由 worker 注入（attach_shared）：回调经同一共享状态计数/登记。
    active_shared: Option<Arc<SharedObservation>>,
    /// candidate seed（仅 `windows_provider_factory_for` 注入）：Observe 前与
    /// 逐读复核完整 FILETIME、HWND/PID/windowRID；discovery provider 无 seed。
    seed: Option<SeedIdentity>,
    /// 声明在最后：字段按声明序 drop，COM 对象先于 CoUninitialize 释放。
    _com: ComApartment,
}

impl Drop for WindowsProvider {
    fn drop(&mut self) {
        // M6：正常/异常退出都在同一 MTA 线程逆序卸载全部 hook，再释放 COM/Uninitialize
        //（字段 drop 在 Drop::drop 之后按声明序执行：COM 对象 → CoUninitialize）。
        if let Some(record) = self.observation.take() {
            self.remove_observation_record(&record);
        }
    }
}

impl WindowsProvider {
    fn check_deadline(&self, deadline: Instant) -> Result<(), UiError> {
        if Instant::now() >= deadline {
            Err(UiError::Timeout)
        } else {
            Ok(())
        }
    }

    /// candidate seed 复核：HWND / PID / 完整 creation FILETIME（100ns，本读重取）/
    /// window RID 逐一相等；任何漂移（进程重启替换、同毫秒不同 FILETIME、窗口 RID
    /// 变化）拒绝。discovery provider（无 seed）恒通过。包/ASAR 固定身份不由本复核
    /// 承担——`window_element` 的 `verify_pinned_identity` 逐次独立强制。
    fn verify_seed(&self, hwnd: isize, pid: u32, window_rid: &[i32]) -> Result<(), UiError> {
        let Some(seed) = &self.seed else {
            return Ok(());
        };
        let creation_filetime = read_process_filetime(pid)?;
        if seed.hwnd != hwnd
            || seed.pid != pid
            || seed.creation_filetime != creation_filetime
            || seed.window_rid.as_slice() != window_rid
        {
            return Err(UiError::Unavailable);
        }
        Ok(())
    }

    /// F1：Control 视角首子步进——生产 closure 直接以 TreeWalker vtable ABI 触发
    /// 真实 COM 调用（`Interface::vtable(..).GetFirstChildElement`，与 windows 0.62
    /// 生成绑定同构）。不先经过高层 `GetFirstChildElement` 的 `Result` 再反推：
    /// windows-core 0.62 `Type::from_abi` 会把 S_OK+null 折成 Err，正常 leaf 会被
    /// 误判为真实失败。失败 HRESULT → Err；成功+null → None（合法尽头）；
    /// 成功+非空 → 接管接口一次（见 `walk_step_with`）。
    fn walker_child(
        &self,
        element: &IUIAutomationElement,
    ) -> Result<Option<IUIAutomationElement>, UiError> {
        walk_step_with(|out| unsafe {
            (Interface::vtable(&self.control_walker).GetFirstChildElement)(
                self.control_walker.as_raw(),
                element.as_raw(),
                out,
            )
        })
    }

    /// 兄弟步进（语义同 `walker_child`，直接调 vtable `GetNextSiblingElement`）。
    fn walker_next_sibling(
        &self,
        element: &IUIAutomationElement,
    ) -> Result<Option<IUIAutomationElement>, UiError> {
        walk_step_with(|out| unsafe {
            (Interface::vtable(&self.control_walker).GetNextSiblingElement)(
                self.control_walker.as_raw(),
                element.as_raw(),
                out,
            )
        })
    }

    /// Raw 实际父（禁 null 折叠：null = 无父 = 链断裂 → Err）。
    fn raw_parent(&self, element: &IUIAutomationElement) -> Result<IUIAutomationElement, UiError> {
        let raw = unsafe { self.raw_walker.GetParentElement(element) };
        match raw {
            Err(_) => Err(UiError::Unavailable),
            Ok(parent) => {
                if parent.as_raw().is_null() {
                    return Err(UiError::Unavailable);
                }
                Ok(parent)
            }
        }
    }

    /// HWND → 元素 + 宿主身份核对（固定包 + ASAR 缓存核对 + HWND PID 核对）。
    fn window_element(
        &self,
        scope: &WindowScope,
        deadline: Instant,
    ) -> Result<(IUIAutomationElement, Vec<i32>, ProcessStamp), UiError> {
        if scope.hwnd == 0 {
            return Err(UiError::Unavailable);
        }
        let hwnd = HWND(scope.hwnd as *mut core::ffi::c_void);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(UiError::Unavailable);
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 {
            return Err(UiError::Unavailable);
        }
        let host = read_process_identity(pid)?;
        // 固定官方包 + ASAR（只读缓存核对）；宿主名不单独放行。
        verify_pinned_identity(&host, deadline)?;
        let element =
            unsafe { self.automation.ElementFromHandle(hwnd) }.map_err(|_| UiError::Unavailable)?;
        // 元素身份交叉核对：元素 PID == HWND PID，NativeWindowHandle == HWND。
        let element_pid = element_process_id(&element)?;
        if element_pid != pid {
            return Err(UiError::Unavailable);
        }
        let native =
            unsafe { element.CurrentNativeWindowHandle() }.map_err(|_| UiError::Unavailable)?;
        if native.0 as isize != scope.hwnd {
            return Err(UiError::Unavailable);
        }
        let rid = element_runtime_id(&element)?;
        // scope 一致性：已钉定字段必须相等；未钉定（0/空）由 provider 填充真实值。
        if scope.pid != 0 && scope.pid != pid {
            return Err(UiError::Unavailable);
        }
        if scope.started_at_ms != 0 && scope.started_at_ms != host.stamp.started_at_ms() {
            // 进程代际漂移（重启替换）：拒绝，不冒充连续。
            return Err(UiError::Unavailable);
        }
        if !scope.package_fingerprint.is_empty()
            && scope.package_fingerprint != PINNED_PACKAGE_FULL_NAME
        {
            return Err(UiError::Unavailable);
        }
        Ok((element, rid, host.stamp))
    }

    /// 组装帧的 live WindowScope：pid/hwnd/包/started_at 取自同源实测；view_path 为
    /// 实际完整 RuntimeId 父链；scope_epoch 用共享观察的真实 epoch（F3：持续观察期
    /// 入站事件已不可逆推进 epoch，透传 caller 旧值会让返回旧画面的帧冒充连续）；
    /// 无活动共享观察时保守透传 caller 值。
    fn live_scope(
        &self,
        caller: &WindowScope,
        host: ProcessStamp,
        view_path: Vec<Vec<i32>>,
    ) -> WindowScope {
        WindowScope {
            pid: host_pid_or_zero(caller.pid),
            started_at_ms: if caller.started_at_ms == 0 {
                host.started_at_ms()
            } else {
                caller.started_at_ms
            },
            hwnd: caller.hwnd,
            package_fingerprint: if caller.package_fingerprint.is_empty() {
                PINNED_PACKAGE_FULL_NAME.to_string()
            } else {
                caller.package_fingerprint.clone()
            },
            view_path,
            scope_epoch: self
                .active_shared
                .as_ref()
                .and_then(|shared| shared.scope_epoch().ok())
                .unwrap_or(caller.scope_epoch),
        }
    }

    /// 候选卡直父链（Raw 实际父链，不含卡）：window → …→ 直父。
    /// 到不了 window 根 / 超深度 → Err（scope 不稳定只能 Err）。
    fn ancestor_chain(
        &self,
        card: &IUIAutomationElement,
        window_rid: &[i32],
        budget: &TraversalBudget,
    ) -> Result<Vec<Vec<i32>>, UiError> {
        let mut chain_rev: Vec<Vec<i32>> = Vec::new();
        let mut current = card.clone();
        for _ in 0..MAX_PARENT_DEPTH {
            budget.visit()?;
            let parent = self.raw_parent(&current)?;
            let rid = element_runtime_id(&parent)?;
            if rid == window_rid {
                chain_rev.push(rid);
                chain_rev.reverse();
                return Ok(chain_rev);
            }
            chain_rev.push(rid);
            current = parent;
        }
        Err(UiError::Unavailable)
    }

    /// 候选卡识别（探测事实联合判定）：
    /// Group + 非空题干名 + 直属子中恰一个（Button + InvokePattern + 名"忽略"）。
    /// 其余直属 Invoke 按钮（跳过/发送）名称互斥。返回 (卡元素, dismiss 元素)。
    fn looks_like_card(
        &self,
        element: &IUIAutomationElement,
        budget: &TraversalBudget,
    ) -> Result<Option<CardCandidate>, UiError> {
        if element_control_type(element)? != UIA_GroupControlTypeId.0 {
            return Ok(None);
        }
        let title = element_name(element)?;
        if title.is_empty() {
            return Ok(None);
        }
        // 直属子节点有界枚举（Control 视角 = 探测证据视角）；兄弟超限 → incomplete。
        let mut dismiss: Option<(IUIAutomationElement, Vec<i32>)> = None;
        let mut child = match self.walker_child(element)? {
            Some(child) => child,
            None => return Ok(None),
        };
        let mut sibling_index = 0usize;
        loop {
            budget.visit()?;
            audit_siblings(sibling_index + 1)?;
            if element_control_type(&child)? == UIA_ButtonControlTypeId.0
                && element_has_invoke_pattern(&child)
            {
                let name = element_name(&child)?;
                let rid = element_runtime_id(&child)?;
                if name == DISMISS_BUTTON_LABEL {
                    // 恰一个直属"忽略"；重复即形态不明 → 拒绝。
                    if dismiss.is_some() {
                        return Err(UiError::Unavailable);
                    }
                    dismiss = Some((child.clone(), rid));
                } else if name == SKIP_BUTTON_LABEL || name == SEND_BUTTON_LABEL {
                    // 其余直属 Invoke 按钮：探测事实内的已知兄弟，不作为 dismiss。
                } else {
                    // 未知直属 Invoke 按钮：保守视为形态不符（不盲认）。
                }
            }
            sibling_index += 1;
            child = match self.walker_next_sibling(&child)? {
                Some(next) => next,
                None => break,
            };
        }
        match dismiss {
            Some((dismiss_element, dismiss_rid)) => {
                // Raw 实际父链复核：dismiss 的 Raw 父必须是本卡（直属，非借位）。
                let raw_parent = self.raw_parent(&dismiss_element)?;
                if element_runtime_id(&raw_parent)? != element_runtime_id(element)? {
                    return Ok(None);
                }
                Ok(Some((
                    element.clone(),
                    element_runtime_id(element)?,
                    dismiss_element,
                    dismiss_rid,
                )))
            }
            None => Ok(None),
        }
    }

    /// 有序 options 读取（探测事实：卡直属子 Group 下的 Text，文档序）。
    /// 截断/读取失败/兄弟超限 → Err（调用方标记 incomplete）；题干同名文本去重。
    fn collect_options(
        &self,
        card: &IUIAutomationElement,
        title: &str,
        budget: &TraversalBudget,
    ) -> Result<Vec<String>, UiError> {
        let mut options: Vec<String> = Vec::new();
        let mut child = match self.walker_child(card)? {
            Some(child) => child,
            None => return Ok(options),
        };
        let mut sibling_index = 0usize;
        loop {
            budget.visit()?;
            audit_siblings(sibling_index + 1)?;
            if element_control_type(&child)? != UIA_ButtonControlTypeId.0 {
                self.collect_texts_under(&child, 0, &mut options, budget)?;
                if options.len() > MAX_CARD_TEXTS {
                    return Err(UiError::Unavailable);
                }
            }
            sibling_index += 1;
            child = match self.walker_next_sibling(&child)? {
                Some(next) => next,
                None => break,
            };
        }
        // 题干同名文本去重（题干同时出现在卡 Name 与正文 Text 的形态）。
        options.retain(|text| text != title && !text.is_empty());
        Ok(options)
    }

    fn collect_texts_under(
        &self,
        element: &IUIAutomationElement,
        depth: usize,
        out: &mut Vec<String>,
        budget: &TraversalBudget,
    ) -> Result<(), UiError> {
        // M2：深度/文本预算超限是 incomplete，不是静默完成。
        audit_depth(depth)?;
        if out.len() > MAX_CARD_TEXTS {
            return Err(UiError::Unavailable);
        }
        budget.visit()?;
        if element_control_type(element)? == uia_text_control_type_id() {
            let text = element_name(element)?;
            out.push(text);
            return Ok(());
        }
        let mut child = match self.walker_child(element)? {
            Some(child) => child,
            None => return Ok(()),
        };
        let mut sibling_index = 0usize;
        loop {
            budget.visit()?;
            audit_siblings(sibling_index + 1)?;
            self.collect_texts_under(&child, depth + 1, out, budget)?;
            sibling_index += 1;
            child = match self.walker_next_sibling(&child)? {
                Some(next) => next,
                None => break,
            };
        }
        Ok(())
    }

    /// 卡内结构 digest：完整 RuntimeId + 控制类型 + 题文/dismiss 的受限子树记录。
    /// 节点/深度超限 → Err（不产假 digest）。
    fn structure_digest_of(
        &self,
        card: &IUIAutomationElement,
        card_rid: &[i32],
        dismiss_rid: &[i32],
        question: &QuestionFingerprint,
        budget: &TraversalBudget,
    ) -> Result<String, UiError> {
        let mut record: Vec<u8> = Vec::new();
        push_str(
            &mut record,
            card_rid
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
                .as_str(),
        );
        push_str(
            &mut record,
            dismiss_rid
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
                .as_str(),
        );
        push_str(&mut record, question.title.as_str());
        push_str(&mut record, question.options.join("\u{1}").as_str());
        self.append_structure_record(card, 0, &mut record, budget)?;
        let digest = Sha256::digest(&record);
        Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn append_structure_record(
        &self,
        element: &IUIAutomationElement,
        depth: usize,
        record: &mut Vec<u8>,
        budget: &TraversalBudget,
    ) -> Result<(), UiError> {
        // M2：结构记录深度超限 → incomplete（不静默截断后冒充完整 digest）。
        if depth > 8 {
            return Err(UiError::Unavailable);
        }
        budget.visit()?;
        let rid = element_runtime_id(element)?;
        let control = element_control_type_name(element)?;
        push_str(
            record,
            rid.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
                .as_str(),
        );
        push_str(record, control.as_str());
        let mut child = match self.walker_child(element)? {
            Some(child) => child,
            None => return Ok(()),
        };
        let mut sibling_index = 0usize;
        loop {
            budget.visit()?;
            audit_siblings(sibling_index + 1)?;
            self.append_structure_record(&child, depth + 1, record, budget)?;
            sibling_index += 1;
            child = match self.walker_next_sibling(&child)? {
                Some(next) => next,
                None => break,
            };
        }
        Ok(())
    }

    /// 全树有界扫描候选卡：0/1 个候选正常；>1 个候选 → Err（单值归属无法确定）。
    /// 外来进程候选（非宿主且无同包代际证明）→ Err（拿不到关系就拒绝）。
    fn scan_cards(
        &self,
        window: &IUIAutomationElement,
        window_rid: &[i32],
        host: &ProcessIdentity,
        caller_scope: &WindowScope,
        budget: &CallBudget,
    ) -> Result<CardScan, UiError> {
        let walk = TraversalBudget::new(MAX_TRAVERSAL_NODES, budget.deadline);
        let mut candidates: Vec<CardCandidate> = Vec::new();
        self.scan_candidates(window, 0, &mut candidates, &walk)?;
        match candidates.len() {
            0 => {
                // 完整扫描无候选：合法的"当前无卡"帧（空 cards 不在此伪装成功，
                // 是否足以证明关闭由 reducer/BoundUiAutomation 的观察结算决定）。
                // M3：空帧同样实读外父链材料（host/window RID 由调用方入基线重验）。
                Ok(CardScan {
                    cards: Vec::new(),
                    parent_chain: Vec::new(),
                    direct_parent: Vec::new(),
                    structure_digest: String::new(),
                    complete: true,
                    card_pid: None,
                })
            }
            1 => {
                let (card, card_rid, dismiss, dismiss_rid) = &candidates[0];
                let candidate_pid = element_process_id(card)?;
                verify_element_process(candidate_pid, host, budget.deadline)?;
                let title = element_name(card)?;
                let options = self.collect_options(card, &title, &walk)?;
                let question = QuestionFingerprint { title, options };
                let chain = self.ancestor_chain(card, window_rid, &walk)?;
                // scope.view_path 已钉定时必须与实际父链逐级相等（连续性核对）。
                if !caller_scope.view_path.is_empty() && caller_scope.view_path != chain {
                    return Err(UiError::Unavailable);
                }
                let structure_digest =
                    self.structure_digest_of(card, card_rid, dismiss_rid, &question, &walk)?;
                let direct_parent = chain.last().cloned().unwrap_or_default();
                Ok(CardScan {
                    cards: vec![NativeCard {
                        runtime_id: card_rid.clone(),
                        dismiss_runtime_id: dismiss_rid.clone(),
                        question,
                        dismiss_is_direct_child: true,
                        invoke_supported: element_has_invoke_pattern(dismiss),
                        // 机器读法不产生题数：面板题数仅由显式用户确认证据注入。
                        native_question_count: None,
                    }],
                    parent_chain: chain,
                    direct_parent,
                    structure_digest,
                    complete: true,
                    card_pid: Some(candidate_pid),
                })
            }
            _ => {
                // 多候选：ProviderFrame 的 direct_parent/structure_digest 单值，
                // 无法确定多候选各自归属 → 保守拒绝（已知限制，见模块文档）。
                Err(UiError::Unavailable)
            }
        }
    }

    fn scan_candidates(
        &self,
        element: &IUIAutomationElement,
        depth: usize,
        candidates: &mut Vec<CardCandidate>,
        walk: &TraversalBudget,
    ) -> Result<(), UiError> {
        // M2：深度超限是 incomplete，不是静默完成（不折断成"没有更多候选"）。
        audit_depth(depth)?;
        walk.visit()?;
        if let Some(found) = self.looks_like_card(element, walk)? {
            if candidates.len() >= 2 {
                return Err(UiError::Unavailable);
            }
            candidates.push(found);
        }
        let mut child = match self.walker_child(element)? {
            Some(child) => child,
            None => return Ok(()),
        };
        let mut sibling_index = 0usize;
        loop {
            walk.visit()?;
            audit_siblings(sibling_index + 1)?;
            self.scan_candidates(&child, depth + 1, candidates, walk)?;
            sibling_index += 1;
            child = match self.walker_next_sibling(&child)? {
                Some(next) => next,
                None => break,
            };
        }
        Ok(())
    }

    /// 完整 RuntimeId 精确重定位（全等比较，禁后缀/索引/文本定位）。
    fn find_by_runtime_id(
        &self,
        window: &IUIAutomationElement,
        runtime_id: &[i32],
        walk: &TraversalBudget,
    ) -> Result<IUIAutomationElement, UiError> {
        let mut matches: Vec<IUIAutomationElement> = Vec::new();
        self.find_by_runtime_id_inner(window, runtime_id, &mut matches, walk)?;
        match matches.len() {
            1 => Ok(matches.remove(0)),
            _ => Err(UiError::StaleElement),
        }
    }

    fn find_by_runtime_id_inner(
        &self,
        element: &IUIAutomationElement,
        runtime_id: &[i32],
        matches: &mut Vec<IUIAutomationElement>,
        walk: &TraversalBudget,
    ) -> Result<(), UiError> {
        walk.visit()?;
        // M2：RuntimeId 读取失败是 incomplete（Err），不能当作"不匹配"继续遍历。
        if element_runtime_id(element)? == runtime_id {
            matches.push(element.clone());
            if matches.len() > 1 {
                return Err(UiError::Unavailable);
            }
        }
        let mut child = match self.walker_child(element)? {
            Some(child) => child,
            None => return Ok(()),
        };
        let mut sibling_index = 0usize;
        loop {
            walk.visit()?;
            audit_siblings(sibling_index + 1)?;
            self.find_by_runtime_id_inner(&child, runtime_id, matches, walk)?;
            sibling_index += 1;
            child = match self.walker_next_sibling(&child)? {
                Some(next) => next,
                None => break,
            };
        }
        Ok(())
    }
}

fn host_pid_or_zero(caller_pid: u32) -> u32 {
    caller_pid
}

fn push_str(buf: &mut Vec<u8>, text: &str) {
    buf.extend_from_slice(&(text.len() as u64).to_le_bytes());
    buf.extend_from_slice(text.as_bytes());
}

/// UIA Text 控制类型常量包装（此处以数值读取控制类型，避免额外绑定差异）。
fn uia_text_control_type_id() -> i32 {
    50020
}

impl PlatformProvider for WindowsProvider {
    fn attach_shared(&mut self, shared: Arc<SharedObservation>) {
        self.active_shared = Some(shared);
    }

    /// 只读发现（NativeDiscovery，零 Invoke）：EnumWindows 顶层枚举（同一模块级
    /// `collect_top_level`；≤ `MAX_DISCOVERY_TOP_LEVEL`，第 257 个即 incomplete
    /// 整体失败，不返回截短集），逐窗 M3 严格三态分类收集器（生产与 seam 测试
    /// 共用同一 `evaluate_candidate_steps`）：已证未打包/外来包名/被拒+独立权威
    /// 无关证据在 image/creation-time 之前早排除；未知错误/无证据被拒整体失败；
    /// Proceed → `window_element` 全套钉定核对（固定 MSIX 包 + ASAR + 元素
    /// PID==HWND PID、NativeWindowHandle==HWND、RuntimeId 实读）。
    /// - 已证不属于钉定包的窗口（含未打包进程）早排除，不读后续步骤；
    /// - 包名命中但 ASAR/身份核对失败 → 整体 Err（未知相关窗口不能冒充无卡，
    ///   身份读取超时/失败不返回截短「complete 候选集」）；
    /// - 合格窗口 ≤ `MAX_DISCOVERY_WINDOWS`，第 9 个合格即整体失败（服务侧同约束复检）；
    /// - 完整 creation FILETIME（100ns）保留进 `DiscoveredWindow`，
    ///   `started_at_ms` 仅同源换算入 scope（不替代完整代际）；
    /// - label = 真实窗口标题（UIA Name 实读；纯文本化与限长由服务层统一处理，
    ///   不以 HWND/PID/RID/窗口编号充当可辨依据）；
    /// - 同一绝对预算贯穿本次发现（不给子步骤重置）。
    fn discover_windows(&mut self, budget: &CallBudget) -> Result<Vec<DiscoveredWindow>, UiError> {
        self.check_deadline(budget.deadline)?;
        let mut collected = TopLevelWindows {
            hwnds: Vec::new(),
            truncated: false,
        };
        let enum_result = unsafe {
            EnumWindows(
                Some(collect_top_level),
                LPARAM(&mut collected as *mut TopLevelWindows as isize),
            )
        };
        if collected.truncated || enum_result.is_err() {
            // 顶层枚举截断或 API 失败：incomplete，不返回截短集。
            return Err(UiError::Unavailable);
        }
        let mut discovered: Vec<DiscoveredWindow> = Vec::new();
        for hwnd_value in collected.hwnds {
            if budget.expired() {
                return Err(UiError::Timeout);
            }
            let hwnd = HWND(hwnd_value as *mut core::ffi::c_void);
            // 枚举后已销毁的窗口：已不是窗口，无从相关，跳过。
            if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                continue;
            }
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            if pid == 0 {
                // 无法归属 → 不可分类：保守失败，不冒充无关。
                return Err(UiError::Unavailable);
            }
            // M3（review 9e11）严格分类收集器（生产与 seam 测试共用同一
            // `evaluate_candidate_steps`，不留镜像副本）：包三态 →（被拒时）独立
            // 权威无关证据 → 步骤编排。已证无关（未打包/外来包名/被拒+独立证据）
            // 在 image/time 之前早排除；未知错误/无证据被拒 → 整体失败（不冒充
            // 无关、不返回截短候选集）；Proceed = 钉定包命中且 image/time 真实
            // 读取全过，继续 window_element 全套钉定核对（MSIX+ASAR+完整代际，
            // 最终安全不降级）。
            let opened = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
            let outcome = match opened {
                Ok(handle) => {
                    let _guard = HandleGuard(handle);
                    query_package_outcome(handle)
                }
                Err(error) => open_process_outcome(&error),
            };
            let independent = matches!(outcome, PackageQueryOutcome::ProcessDenied)
                && toolhelp_unrelated_evidence(pid, budget.deadline);
            let mut creation_filetime = None;
            {
                // 分类闭包收进独立作用域：借用随块结束释放，块外即可读取实测代际。
                let mut image = || -> Result<String, UiError> {
                    if budget.expired() {
                        return Err(UiError::Timeout);
                    }
                    let handle =
                        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                            .map_err(|_| UiError::Unavailable)?;
                    let _guard = HandleGuard(handle);
                    read_process_image_path(handle)
                };
                let mut time = || -> Result<u64, UiError> {
                    if budget.expired() {
                        return Err(UiError::Timeout);
                    }
                    // 单次 match：借用成功值缓存（Result 不被 ok() 提前移动）。
                    match read_process_filetime(pid) {
                        Ok(filetime) => {
                            creation_filetime = Some(filetime);
                            Ok(filetime)
                        }
                        Err(error) => Err(error),
                    }
                };
                let (verdict, _, _) = evaluate_candidate_steps(
                    PINNED_PACKAGE_FULL_NAME,
                    independent,
                    &mut || outcome.clone(),
                    &mut image,
                    &mut time,
                );
                match verdict {
                    CandidateVerdict::ExcludeUnrelated => continue,
                    CandidateVerdict::FailWhole => {
                        // 不可分类的相关窗口 → 整体失败（不冒充无关、不返回截短集）。
                        return Err(UiError::Unavailable);
                    }
                    CandidateVerdict::Proceed => {}
                }
            }
            // 完整代际必须保留 100ns 值（started_at_ms 只是同源换算，不替代）；
            // 已由分类器 creation-time 步骤真实读取。
            let creation_filetime = creation_filetime.ok_or(UiError::Unavailable)?;
            // window_element 同源校验：scope 钉定字段未填 → 由其填充真实值。
            let probe = WindowScope {
                pid: 0,
                started_at_ms: 0,
                hwnd: hwnd_value,
                package_fingerprint: String::new(),
                view_path: Vec::new(),
                scope_epoch: 0,
            };
            let (element, window_rid, _stamp) = self.window_element(&probe, budget.deadline)?;
            // label = 真实窗口标题（UIA Name 实读）；读取失败 → 整体失败（包内
            // 相关窗口的标题不可得不能冒充「无标题可忽略」）。
            let label = element_name(&element)?;
            discovered.push(DiscoveredWindow {
                scope: WindowScope {
                    pid,
                    started_at_ms: filetime_to_unix_ms(creation_filetime),
                    hwnd: hwnd_value,
                    package_fingerprint: PINNED_PACKAGE_FULL_NAME.to_string(),
                    view_path: vec![window_rid.clone()],
                    scope_epoch: 0,
                },
                host_creation_filetime: creation_filetime,
                window_runtime_id: window_rid,
                label,
            });
            if discovered.len() > MAX_DISCOVERY_WINDOWS {
                // 第 9 个合格窗口 → 整体失败（不返回截短候选集）。
                return Err(UiError::Unavailable);
            }
        }
        Ok(discovered)
    }

    fn observe(
        &mut self,
        scope: &WindowScope,
        sink: EventSink,
        budget: &CallBudget,
    ) -> Result<ObservationId, UiError> {
        self.check_deadline(budget.deadline)?;
        // 一个窗口观察仅容纳一个 active attempt（串行由既有 worker/观察状态保证）。
        if self.observation.is_some() {
            return Err(UiError::Unavailable);
        }
        let shared = self.active_shared.clone().ok_or(UiError::Unavailable)?;
        let (window, window_rid, _stamp) = self.window_element(scope, budget.deadline)?;
        // Observe 前 seed 复核（candidate 工厂绑定窗口身份漂移即拒绝）；「后」半边
        // 由 read_frame 逐读 seed + identity_baseline 重验构成。
        self.verify_seed(scope.hwnd, element_process_id(&window)?, &window_rid)?;
        // M5 覆盖审计：结构/属性/Selection/SelectionItem 四面齐全才注册。
        let coverage = [
            SubscriptionKind::StructureChanged,
            SubscriptionKind::PropertyChanged,
            SubscriptionKind::SelectionInvalidated,
            SubscriptionKind::SelectionItemEvent,
        ];
        audit_subscription_coverage(&coverage)?;
        self.next_observation_seq += 1;
        let observation = ObservationId(self.next_observation_seq);
        // M4：注册含 RuntimeId 的真实 cache request；回调只读缓存（缺 = Unknown）。
        let cache_request =
            unsafe { self.automation.CreateCacheRequest() }.map_err(|_| UiError::Unavailable)?;
        unsafe { cache_request.AddProperty(UIA_RuntimeIdPropertyId) }
            .map_err(|_| UiError::Unavailable)?;
        // 专用 handler（结构 / 属性+选择），不用通用事件冒充结构事件。
        let structure_handler: IUIAutomationStructureChangedEventHandler = StructureForwarder {
            shared: shared.clone(),
            sink: sink.clone(),
        }
        .into();
        let forwarder = PropertySelectionForwarder {
            shared: shared.clone(),
            sink,
        };
        let unknown: windows::core::IUnknown = forwarder.into();
        let property_handler: IUIAutomationPropertyChangedEventHandler =
            unknown.cast().map_err(|_| UiError::Unavailable)?;
        let selection_handler: IUIAutomationEventHandler =
            unknown.cast().map_err(|_| UiError::Unavailable)?;
        let mut manifest = SubscriptionManifest::new();
        let automation_events: Vec<UIA_EVENT_ID>;
        // 注册顺序：结构 → 属性 → 自动化事件；任一步失败即逆序回滚已注册项，
        // 不留半订阅（M6）。
        unsafe {
            self.automation
                .AddStructureChangedEventHandler(
                    &window,
                    TreeScope_Subtree,
                    &cache_request,
                    &structure_handler,
                )
                .map_err(|_| UiError::Unavailable)?;
            manifest.record_success(SubscriptionEntry {
                kind: SubscriptionKind::StructureChanged,
                target_rid: window_rid.clone(),
            });
            if self
                .automation
                .AddPropertyChangedEventHandlerNativeArray(
                    &window,
                    TreeScope_Subtree,
                    &cache_request,
                    &property_handler,
                    OBSERVED_PROPERTY_IDS,
                )
                .is_err()
            {
                let _ = self
                    .automation
                    .RemoveStructureChangedEventHandler(&window, &structure_handler);
                return Err(UiError::Unavailable);
            }
            manifest.record_success(SubscriptionEntry {
                kind: SubscriptionKind::PropertyChanged,
                target_rid: window_rid.clone(),
            });
            // M5：Selection Invalidated + SelectionItem selected/added/removed 全注册。
            let mut committed: Vec<UIA_EVENT_ID> = Vec::new();
            let mut all_ok = true;
            for event_id in [
                UIA_Selection_InvalidatedEventId,
                SELECTION_ITEM_EVENT_IDS[0],
                SELECTION_ITEM_EVENT_IDS[1],
                SELECTION_ITEM_EVENT_IDS[2],
            ] {
                if self
                    .automation
                    .AddAutomationEventHandler(
                        event_id,
                        &window,
                        TreeScope_Subtree,
                        &cache_request,
                        &selection_handler,
                    )
                    .is_err()
                {
                    all_ok = false;
                    break;
                }
                committed.push(event_id);
                // 清单按注册顺序记录（rollback_plan 逆序 = 后注册先撤）。
                let kind = if event_id == UIA_Selection_InvalidatedEventId {
                    SubscriptionKind::SelectionInvalidated
                } else {
                    SubscriptionKind::SelectionItemEvent
                };
                manifest.record_success(SubscriptionEntry {
                    kind,
                    target_rid: window_rid.clone(),
                });
            }
            if !all_ok {
                // 逆序回滚：后注册先撤。
                for event_id in committed.iter().rev() {
                    let _ = self.automation.RemoveAutomationEventHandler(
                        *event_id,
                        &window,
                        &selection_handler,
                    );
                }
                let _ = self
                    .automation
                    .RemovePropertyChangedEventHandler(&window, &property_handler);
                let _ = self
                    .automation
                    .RemoveStructureChangedEventHandler(&window, &structure_handler);
                return Err(UiError::Unavailable);
            }
            automation_events = committed;
        }
        self.observation = Some(ObservationRecord {
            observation: observation.clone(),
            shared,
            window,
            window_rid,
            // F7：新 observe 从 None 建立（基线归 observation，不继承旧观察）。
            identity_baseline: None,
            manifest,
            automation_events,
            structure: structure_handler,
            property: property_handler,
            selection: selection_handler,
            cache_request,
        });
        Ok(observation)
    }

    fn capture(
        &mut self,
        scope: &WindowScope,
        budget: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        // 绑定后读取：与 preview 同一读路径（M3 基线重验/单值扫描均在此）。
        self.read_frame(scope, budget)
    }

    fn capture_preview(
        &mut self,
        scope: &WindowScope,
        _shared: &Arc<SharedObservation>,
        budget: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        // A：独立只读预览（先于 Book 签证；无 AttemptContext/证据注入、无 Invoke 权），
        // 走与 capture 完全相同的真实读路径。
        self.read_frame(scope, budget)
    }

    fn invoke_dismiss(
        &mut self,
        target: &InvokeTarget,
        ctx: &AttemptContext,
        admission: &FinalAdmission,
        budget: &CallBudget,
    ) -> Result<(), UiError> {
        self.check_deadline(budget.deadline)?;
        let walk = TraversalBudget::new(MAX_TRAVERSAL_NODES, budget.deadline);
        // 重读作用域全部守卫：窗口身份（HWND PID / 固定包 / ASAR 缓存核对 / 代际）。
        let (window, window_rid, _stamp) = self.window_element(&target.scope, budget.deadline)?;
        let host = read_process_identity(element_process_id(&window)?)?;
        // 订阅健康守卫：观察失效（in-flight / 溢出 / 毒化 / 已卸载）不 Invoke。
        let record = self.observation.as_ref().ok_or(UiError::Unavailable)?;
        let cp = record
            .shared
            .checkpoint()
            .map_err(|_| UiError::Unavailable)?;
        if cp.poisoned || cp.overflow || record.shared.is_closed().unwrap_or(true) {
            return Err(UiError::Unavailable);
        }
        // B2：完整重读帧先于 pattern（全文/options/结构/父链/身份/count），作为
        // measured 交给 FinalAdmission 与 ctx.expected_frame 全量比对。
        let measured = self.read_frame(&target.scope, budget)?;
        // 完整 RuntimeId 精确重定位卡与 dismiss（全等，禁后缀）。
        let card = self.find_by_runtime_id(&window, &target.card_runtime_id, &walk)?;
        let dismiss = self.find_by_runtime_id(&window, &target.dismiss_runtime_id, &walk)?;
        // 直属关系复核：dismiss 的 Raw 实际父必须是本卡（不借 parent）。
        let raw_parent = self
            .raw_parent(&dismiss)
            .map_err(|_| UiError::StaleElement)?;
        if element_runtime_id(&raw_parent)? != target.card_runtime_id {
            return Err(UiError::StaleElement);
        }
        // 卡形态复核（探测事实联合判定；同名盲 Invoke 禁止）。
        if element_control_type(&card)? != UIA_GroupControlTypeId.0 {
            return Err(UiError::StaleElement);
        }
        if element_control_type(&dismiss)? != UIA_ButtonControlTypeId.0
            || element_name(&dismiss)? != DISMISS_BUTTON_LABEL
            || !element_has_invoke_pattern(&dismiss)
        {
            return Err(UiError::StaleElement);
        }
        // 卡进程身份放行（宿主 / 同包代际子进程）。
        let card_pid = element_process_id(&card)?;
        verify_element_process(card_pid, &host, budget.deadline)?;
        // 父链连续性复核：实际祖先链必须与绑定 scope.view_path 逐级相等。
        let chain = self.ancestor_chain(&card, &window_rid, &walk)?;
        if !target.scope.view_path.is_empty() && target.scope.view_path != chain {
            return Err(UiError::StaleElement);
        }
        // 昂贵获取完成：取真实 InvokePattern（UIA_InvokePatternId）。
        let pattern = element_invoke_pattern(&dismiss)?;
        self.check_deadline(budget.deadline)?;
        // 最终准入：Fake/Windows 同走 admission.enter(measured)——live recheck/
        // 排空事件 checkpoint/与 expected_frame 全量比对/Gate CAS 之后紧邻 Invoke。
        let _ = ctx;
        let _admission = admission.enter(&measured)?;
        // COM 在途不能安全撤回；结果如实返回（迟到成功不升级回执由 gate.finish 承担）。
        unsafe { pattern.Invoke() }.map_err(|_| UiError::Unavailable)
    }

    fn unobserve(&mut self, observation: &ObservationId) {
        if let Some(record) = self.observation.take() {
            if record.observation != *observation {
                // 非当前观察：恢复并忽略（幂等清理）。
                self.observation = Some(record);
                return;
            }
            self.remove_observation_record(&record);
        }
    }
}

impl WindowsProvider {
    /// M6：同一 MTA 线程逆序卸载全部已注册 hook（事件 → 属性 → 结构），随后关闭
    /// 共享观察（alive=false，竞态回调静默丢弃）。失败仅忽略——观察态已卸载。
    fn remove_observation_record(&self, record: &ObservationRecord) {
        let mut pending_events = record.automation_events.clone();
        for entry in record.manifest.rollback_plan() {
            match entry.kind {
                SubscriptionKind::StructureChanged => unsafe {
                    let _ = self
                        .automation
                        .RemoveStructureChangedEventHandler(&record.window, &record.structure);
                },
                SubscriptionKind::PropertyChanged => unsafe {
                    let _ = self
                        .automation
                        .RemovePropertyChangedEventHandler(&record.window, &record.property);
                },
                SubscriptionKind::SelectionInvalidated | SubscriptionKind::SelectionItemEvent => {
                    if let Some(event_id) = pending_events.pop() {
                        unsafe {
                            let _ = self.automation.RemoveAutomationEventHandler(
                                event_id,
                                &record.window,
                                &record.selection,
                            );
                        }
                    }
                }
            }
        }
        record.shared.close();
    }

    /// 统一真实读路径（capture / capture_preview / invoke measured 共用）：
    /// HWND 身份核对 → 全树有界扫描（M2 审计）→ M3 身份基线重验（含空帧）→
    /// 共享观察事实（healthy/seq）组装 ProviderFrame。
    fn read_frame(
        &mut self,
        scope: &WindowScope,
        budget: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        self.check_deadline(budget.deadline)?;
        let record = self.observation.as_ref().ok_or(UiError::Unavailable)?;
        let observation_id = record.observation.clone();
        let shared = record.shared.clone();
        let (window, window_rid, stamp) = self.window_element(scope, budget.deadline)?;
        let window_pid = element_process_id(&window)?;
        // 逐读 seed 复核（candidate 工厂绑定窗口身份漂移即拒绝；完整 FILETIME 本读重取）。
        self.verify_seed(scope.hwnd, window_pid, &window_rid)?;
        let host = read_process_identity(window_pid)?;
        let scan = self.scan_cards(&window, &window_rid, &host, scope, budget)?;
        // M3/F7：身份基线归本 observation（unobserve 随记录丢弃，新 observe 从
        // None 重建）。逐次实读重验 host/renderer 完整 creation FILETIME、PID、包、
        // 实际 image path、window RID；空帧同样实读外父链材料（window RID/host）。
        let renderer = match scan.card_pid {
            Some(pid) if pid != host.pid => {
                let child = read_process_identity(pid)?;
                Some(ProcessIdentityFact {
                    pid: child.pid,
                    creation_filetime: child.stamp.creation_filetime,
                    package_full_name: child.package_full_name.clone(),
                    image_path: child.image_path.clone(),
                })
            }
            _ => None,
        };
        let current = IdentityBaseline {
            host: ProcessIdentityFact {
                pid: host.pid,
                creation_filetime: host.stamp.creation_filetime,
                package_full_name: host.package_full_name.clone(),
                image_path: host.image_path.clone(),
            },
            renderer,
            window_rid: window_rid.clone(),
        };
        let record = self.observation.as_mut().ok_or(UiError::Unavailable)?;
        // SPEC6 窄事务缝：生产 read_frame 直接经 `update_identity_baseline` 提交
        // 基线 slot——Ok 才更新；Err 保留旧基线（不 take 不清空）并毒化本
        // observation（同 observation 下一次读仍拒：A→B 拒 → B 仍拒，含同毫秒
        // 不同完整 FILETIME；仅新 observe 从 None 重建）。判定复用
        // `settle_identity_baseline`，身份规则不重写。
        update_identity_baseline(
            &mut record.identity_baseline,
            &current,
            budget.deadline,
            proven_identity_baseline,
            || {
                let _ = shared.mark_poisoned();
            },
        )?;
        let live_scope = self.live_scope(
            scope,
            stamp,
            // 空扫描无候选 → 无父链可核对；已钉定的 view_path 保持 caller 值。
            if scan.parent_chain.is_empty() {
                scope.view_path.clone()
            } else {
                scan.parent_chain
            },
        );
        // 观察事实来自共享状态（与回调同一权威源）：无 in-flight/溢出/毒化且未卸载
        // 才算 healthy；seq 为 ingress 已登记序号。
        let cp = shared.checkpoint().map_err(|_| UiError::Unavailable)?;
        let observation_healthy =
            !shared.is_closed()? && !cp.poisoned && !cp.overflow && cp.in_flight == 0;
        Ok(ProviderFrame {
            snapshot: NativeCardSnapshot {
                scope: live_scope,
                complete: scan.complete,
                cards: scan.cards,
            },
            observation: observation_id,
            observation_healthy,
            event_seq: cp.ingress_seq,
            direct_parent: scan.direct_parent,
            structure_digest: scan.structure_digest,
            // 当前真实 profile 无独立于待删卡且导航敏感的上下文见证：
            // 恒 None（不签 IndependentContext；仅卡消失必须 Unresolved）。
            context_witness: None,
        })
    }
}

/// 监听的属性事件（M5 完整清单：名称 / 离屏 / 启用 / Value）。
const OBSERVED_PROPERTY_IDS: &[UIA_PROPERTY_ID] = &[
    UIA_NamePropertyId,
    UIA_IsOffscreenPropertyId,
    UIA_IsEnabledPropertyId,
    UIA_ValueValuePropertyId,
];

/// M5 SelectionItem 事件清单（selected / added / removed；Selection Invalidated 已在
/// observe 注册）。事件注册调用的清单化接线与部分失败逆序回滚在【实现段】经
/// `SubscriptionManifest`（helper.rs 纯缝）完成；本阶段先钉定完整清单数据。
const SELECTION_ITEM_EVENT_IDS: &[UIA_EVENT_ID] = &[
    UIA_SelectionItem_ElementSelectedEventId,
    UIA_SelectionItem_ElementAddedToSelectionEventId,
    UIA_SelectionItem_ElementRemovedFromSelectionEventId,
];

/// MTA 内部构造（discovery / candidate 两工厂共用同一构造函数）：CoInitializeEx(MTA) +
/// CUIAutomation 实例化 + Raw/Control walker。初始化失败不发布可用 backend；
/// COM 对象不跨线程，只传 DTO。包/ASAR 身份核对不在构造期（构造无窗口事实），
/// 由 `window_element`/`verify_pinned_identity` 在每次使用时独立强制。
fn mta_provider(seed: Option<SeedIdentity>) -> Result<WindowsProvider, UiError> {
    let com = ComApartment::init_mta()?;
    // MTA 内创建 UIA 客户端（INPROC）。
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| UiError::Unavailable)?;
    let raw_walker = unsafe { automation.RawViewWalker() }.map_err(|_| UiError::Unavailable)?;
    let control_walker =
        unsafe { automation.ControlViewWalker() }.map_err(|_| UiError::Unavailable)?;
    Ok(WindowsProvider {
        automation,
        raw_walker,
        control_walker,
        observation: None,
        next_observation_seq: 0,
        active_shared: None,
        seed,
        _com: com,
    })
}

/// 真实工厂（无 seed，供只读 discovery worker 使用）：在 worker 线程内执行。
/// 初始化失败不发布可用 backend；COM 对象不跨线程，只传 DTO。
pub(crate) fn windows_provider_factory(
) -> impl FnOnce() -> Result<Box<dyn PlatformProvider>, UiError> + Send + 'static {
    move || Ok(Box::new(mta_provider(None)?) as Box<dyn PlatformProvider>)
}

/// candidate 工厂：为候选窗口保存 seed（完整 creation FILETIME 100ns / HWND / PID /
/// windowRID）。Observe 前与逐读复核 seed（`verify_seed`：完整 FILETIME 代际，
/// 非 ms 近似；HWND/PID/RID 漂移拒绝）；固定包/ASAR 身份仍由
/// `window_element` 的钉定校验逐次独立强制。复用与
/// [`windows_provider_factory`] 完全相同的 MTA 内部构造函数。
pub(crate) fn windows_provider_factory_for(
    window: DiscoveredWindow,
) -> impl FnOnce() -> Result<Box<dyn PlatformProvider>, UiError> + Send + 'static {
    let seed = SeedIdentity {
        hwnd: window.scope.hwnd,
        pid: window.scope.pid,
        creation_filetime: window.host_creation_filetime,
        window_rid: window.window_runtime_id.clone(),
    };
    move || Ok(Box::new(mta_provider(Some(seed))?) as Box<dyn PlatformProvider>)
}

// VARIANT 仅出现在属性回调签名（newvalue: &VARIANT）中；DTO 不携带值，不深读，
// 借用参数的所有权归 UIA 回调层。

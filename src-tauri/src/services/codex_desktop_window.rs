//! Mac 原窗口观察的保守对位。这里不导航、不改变焦点，也不提供写动作。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub pid: u32,
    pub launch_generation: u64,
    pub login_session: u64,
}

impl HostIdentity {
    fn is_complete(&self) -> bool {
        self.pid != 0 && self.launch_generation != 0 && self.login_session != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowObservation {
    pub generation: u64,
    /// 仅接受提供方实际核对的原线程标识，不接受标题、位置或缓存猜测。
    pub loaded_thread: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub host: HostIdentity,
    pub signature_verified: bool,
    pub windows: Vec<WindowObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationError {
    Unsupported,
    NotAuthorized,
    TimedOut,
    HostMismatch,
    ThreadUnknown,
}

/// 提供方只观察已授权、已选定的进程。测试可验证否定条件而不读真实窗口。
pub trait WindowProvider {
    fn observe(&self, target: &HostIdentity) -> Result<Snapshot, ObservationError>;
}

pub fn unique_loaded_window(
    provider: &impl WindowProvider,
    authorized_host: &HostIdentity,
    thread: &str,
) -> Result<u64, ObservationError> {
    if !authorized_host.is_complete() {
        return Err(ObservationError::HostMismatch);
    }
    if thread.trim().is_empty() {
        return Err(ObservationError::ThreadUnknown);
    }
    let snapshot = provider.observe(authorized_host)?;
    if !snapshot.signature_verified || &snapshot.host != authorized_host {
        return Err(ObservationError::HostMismatch);
    }
    let mut matching = snapshot
        .windows
        .iter()
        .filter(|window| window.loaded_thread.as_deref() == Some(thread));
    let first = matching.next().ok_or(ObservationError::ThreadUnknown)?;
    if first.generation == 0
        || matching.next().is_some()
        || snapshot
            .windows
            .iter()
            .filter(|window| window.generation == first.generation)
            .count()
            != 1
    {
        return Err(ObservationError::ThreadUnknown);
    }
    Ok(first.generation)
}

/// 系统 AX 只读探针。调用方必须在正常授权的隔离登录环境中，先确认官方进程签名和
/// 宿主身份。这里只读窗口数量，不读标题、正文、草稿或猜测原线程标识。
#[cfg(target_os = "macos")]
pub struct MacWindowProvider<F> {
    verify_host: F,
}

#[cfg(target_os = "macos")]
impl<F> MacWindowProvider<F>
where
    F: Fn(&HostIdentity) -> bool,
{
    pub fn new(verify_host: F) -> Self {
        Self { verify_host }
    }
}

#[cfg(target_os = "macos")]
impl<F: Fn(&HostIdentity) -> bool> WindowProvider for MacWindowProvider<F> {
    fn observe(&self, target: &HostIdentity) -> Result<Snapshot, ObservationError> {
        use std::ffi::c_void;
        #[link(name = "ApplicationServices", kind = "framework")]
        extern "C" {
            fn AXIsProcessTrusted() -> bool;
            fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
            fn AXUIElementSetMessagingTimeout(element: *const c_void, timeout: f32) -> i32;
            fn AXUIElementGetAttributeValueCount(
                object: *const c_void,
                attribute: *const c_void,
                count: *mut isize,
            ) -> i32;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CFStringCreateWithCString(
                allocator: *const c_void,
                string: *const std::ffi::c_char,
                encoding: u32,
            ) -> *const c_void;
            fn CFRelease(object: *const c_void);
        }
        struct Owned(*const c_void);
        impl Drop for Owned {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { CFRelease(self.0) }
                }
            }
        }
        let pid = i32::try_from(target.pid).map_err(|_| ObservationError::HostMismatch)?;
        if !target.is_complete() || !(self.verify_host)(target) {
            return Err(ObservationError::HostMismatch);
        }
        // 正常 AX 查询不弹授权对话框，不创建或激活目标进程。
        if !unsafe { AXIsProcessTrusted() } {
            return Err(ObservationError::NotAuthorized);
        }
        let app = Owned(unsafe { AXUIElementCreateApplication(pid) });
        let windows = Owned(unsafe {
            CFStringCreateWithCString(std::ptr::null(), c"AXWindows".as_ptr(), 0x0800_0100)
        });
        if app.0.is_null() || windows.0.is_null() {
            return Err(ObservationError::ThreadUnknown);
        }
        if unsafe { AXUIElementSetMessagingTimeout(app.0, 0.2) } != 0 {
            return Err(ObservationError::TimedOut);
        }
        let mut count = 0;
        let status = unsafe { AXUIElementGetAttributeValueCount(app.0, windows.0, &mut count) };
        if status == -25204 {
            return Err(ObservationError::TimedOut);
        }
        if status != 0 || !(0..=16).contains(&count) {
            return Err(ObservationError::ThreadUnknown);
        }
        if !(self.verify_host)(target) {
            return Err(ObservationError::HostMismatch);
        }
        if !unsafe { AXIsProcessTrusted() } {
            return Err(ObservationError::NotAuthorized);
        }
        Ok(Snapshot {
            host: target.clone(),
            signature_verified: true,
            windows: vec![
                WindowObservation {
                    generation: 0,
                    loaded_thread: None
                };
                count as usize
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Result<Snapshot, ObservationError>);
    impl WindowProvider for Fake {
        fn observe(&self, _: &HostIdentity) -> Result<Snapshot, ObservationError> {
            self.0.clone()
        }
    }

    #[test]
    fn window_generation_collision_never_identifies_an_exact_window() {
        let host = HostIdentity {
            pid: 7,
            launch_generation: 3,
            login_session: 1,
        };
        let provider = Fake(Ok(Snapshot {
            host: host.clone(),
            signature_verified: true,
            windows: vec![
                WindowObservation {
                    generation: 2,
                    loaded_thread: Some("A".into()),
                },
                WindowObservation {
                    generation: 2,
                    loaded_thread: Some("B".into()),
                },
            ],
        }));
        assert_eq!(
            unique_loaded_window(&provider, &host, "A"),
            Err(ObservationError::ThreadUnknown)
        );
    }

    #[test]
    fn unknown_host_identity_never_calls_the_window_provider() {
        struct NeverObserve;
        impl WindowProvider for NeverObserve {
            fn observe(&self, _: &HostIdentity) -> Result<Snapshot, ObservationError> {
                panic!("未经完整身份绑定不读取窗口");
            }
        }
        let host = HostIdentity {
            pid: 7,
            launch_generation: 0,
            login_session: 1,
        };
        assert_eq!(
            unique_loaded_window(&NeverObserve, &host, "A"),
            Err(ObservationError::HostMismatch)
        );
    }

    #[test]
    fn only_same_generation_verified_unique_loaded_thread_matches() {
        let host = HostIdentity {
            pid: 7,
            launch_generation: 3,
            login_session: 1,
        };
        let window = WindowObservation {
            generation: 2,
            loaded_thread: Some("A".into()),
        };
        let observe = |host: HostIdentity, signature_verified, windows| {
            Fake(Ok(Snapshot {
                host,
                signature_verified,
                windows,
            }))
        };
        assert_eq!(
            unique_loaded_window(
                &observe(host.clone(), true, vec![window.clone()]),
                &host,
                "A"
            ),
            Ok(2)
        );
        for error in [
            ObservationError::Unsupported,
            ObservationError::NotAuthorized,
            ObservationError::TimedOut,
        ] {
            assert_eq!(
                unique_loaded_window(&Fake(Err(error)), &host, "A"),
                Err(error)
            );
        }
        assert_eq!(
            unique_loaded_window(
                &observe(host.clone(), false, vec![window.clone()]),
                &host,
                "A"
            ),
            Err(ObservationError::HostMismatch)
        );
        for replaced in [
            HostIdentity {
                launch_generation: 4,
                ..host.clone()
            },
            HostIdentity {
                login_session: 2,
                ..host.clone()
            },
        ] {
            assert_eq!(
                unique_loaded_window(&observe(replaced, true, vec![window.clone()]), &host, "A"),
                Err(ObservationError::HostMismatch)
            );
        }
        for windows in [
            vec![],
            vec![window.clone(), window.clone()],
            vec![WindowObservation {
                generation: 2,
                loaded_thread: None,
            }],
        ] {
            assert_eq!(
                unique_loaded_window(&observe(host.clone(), true, windows), &host, "A"),
                Err(ObservationError::ThreadUnknown)
            );
        }
        assert_eq!(
            unique_loaded_window(&observe(host.clone(), true, vec![window]), &host, "B"),
            Err(ObservationError::ThreadUnknown)
        );
    }
}

// ============================================================
// 固定多窗口逐窗绑定（Task 1，2026-11-27，纯判定）
// ============================================================

// 一次逐窗观察中的身份字段。窗口身份与序号、标题、焦点均无关。
//
// ============================================================
// 逐窗部分在本构建**不适用**（规格 §8）
//
// `WindowIdentity` / `VerifiedHostProvenance` / `MultiWindowSnapshot` /
// `resolve_window_target` / `evaluate_multiwindow_frame_text` 全部标记为
// **instrument-only**：它们只用于取证与回归，**不得**出现在产品写入路径上。
//
// 依据：真机实测本构建的 `AXWindow` 不暴露任何可作身份的属性
// （`AXWindowNumber`/`AXIdentifier`/`AXDOMIdentifier` 等均返回不支持），
// 所以 v1 的"按窗口绑定"前提不成立。产品路径只用 `resolve_current_target`
// 的单视图判定，按**会话行身份**寻址。
//
// 未来若出现新的窗口身份信号，也须重新取证，不自动启用。
// ============================================================

/// 身份源只记录**实际读到的属性值**，不推导持久身份，也不证明跨重启稳定；缺少、
/// 空值或快照内相撞时，逐窗绑定必须拒绝。`generation` 仅是本轮观察代际，不是 AX 属性，
/// 也不能证明窗口在两次读取之间没有变化。
///
/// **instrument-only：本构建不适用（规格 §8）。** 不要在产品路径上使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowIdentity {
    pub identity: String,
    pub source: WindowIdentitySource,
    pub generation: u64,
}

/// 逐窗 AX 报告所采用的窗口身份来源。记录属性名，不表示该来源跨重启稳定。
///
/// 它不能单独证明持久窗口身份或写入资格；最终仍须核对批准键、代际与快照内唯一性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowIdentitySource {
    WindowNumber,
    AxIdentifier,
    Other,
}

impl WindowIdentitySource {
    fn from_observed(source: &str) -> Self {
        match source {
            "AXWindowNumber" => Self::WindowNumber,
            "AXIdentifier" => Self::AxIdentifier,
            _ => Self::Other,
        }
    }
}

/// 已批准的具体窗口目标键。除了宿主和线程，还固定窗口身份及两套代际。
///
/// `binding_generation` 供 Task 2 的动作账本区分同一目标的重复批准；Task 1 只要求非零，
/// 不证明它已持久化，也不授权任何写入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedWindowTarget<'a> {
    pub host: HostIdentity,
    pub window_identity: &'a str,
    pub window_generation: u64,
    pub binding_generation: u64,
    pub workspace_key: &'a str,
    pub thread_id: &'a str,
    pub title_hash: &'a str,
    pub project_label_hash: &'a str,
}

/// 一个**具体窗口**的 AX 自报；`window_count` 是本次快照总数，不是该窗的序号。
///
/// 这是新增的逐窗数据面，旧 [`ProbeReport`] / [`resolve_current_target`] 不消费它，
/// 因此不会放宽旧单窗保护。`internally_consistent` 只核帧内数量、预算与宿主字段，
/// 不能证明身份稳定、线程唯一，更不能作为写门。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowProbeReport {
    pub host: HostIdentity,
    pub window_count: usize,
    pub window_identity: String,
    pub identity_source: WindowIdentitySource,
    pub window_generation: u64,
    pub window_title_hash: String,
    pub frontmost_observed: Option<bool>,
    pub rows: Vec<SidebarRow>,
    pub counts: EnumerationCounts,
    pub groups_available: bool,
}

impl WindowProbeReport {
    /// 该**窗自己的**帧是否内部自洽，并属于完整、可信的多窗口快照。
    ///
    /// 它不证明身份跨帧稳定、线程归属或可写；`frontmost_observed` 只作记录，不参与判定。
    pub fn internally_consistent(&self) -> bool {
        let c = &self.counts;
        self.window_count > 0
            && c.rows_enumerated == self.rows.len()
            && c.structural_unknown == 0
            && !c.budget_hit
            && !c.read_failed
            && c.title_recheck_stable
            && c.nodes_visited > 0
            && c.ax_calls > 0
            && self.host.pid != 0
            && self.host.launch_generation != 0
            && self.host.login_session != 0
            && !self.window_identity.trim().is_empty()
            && self.identity_source != WindowIdentitySource::Other
            && self.window_generation != 0
    }
}

/// 逐窗绑定结果的四种语义。与旧 [`TargetBinding`] 分开，避免任何单窗门被间接复用。
///
/// 即使是 [`WindowTargetBinding::Matched`]，也只表示本帧内唯一候选对上了批准键，
/// **不是写门放行依据**，不证明动作已受理或正式消息已产生。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowTargetBinding {
    Matched,
    OtherThread,
    Ambiguous,
    Unverifiable,
}

/// 已解析的多窗口帧。每个块只装自己窗口的身份、代际、标题与侧栏行。
///
/// 本结构不持有 AX 引用、不执行动作，也不证明任何窗口处于可写或空闲状态。
///
/// **`signature_verified` 绝不由解析器置真**（复审 Critical B）。帧文本是外部输入，
/// 若解析器自己填 `true`，就等于"被观测数据自证可信"——与本模块此前修掉的
/// `approved_target_for` 宿主自证是同一类缺陷。解析一律产出 `false`；可信来源须由调用方
/// 在帧之外独立取得（见 [`VerifiedHostProvenance`]），并在 [`resolve_window_target`] 中
/// 与帧内容分开比对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiWindowSnapshot {
    pub host: HostIdentity,
    pub signature_verified: bool,
    pub windows: Vec<WindowProbeReport>,
}

/// 在帧之外**独立取得**的宿主可信证据。
///
/// 它回答的是"这个 PID / 启动代际 / 登录会话是否属于已验签的官方 App 实例"，
/// 而帧文本回答的是"该实例此刻报告了哪些窗口"。两者必须来自不同来源，否则
/// 逐窗绑定就退化为自证。本类型只能由调用方在完成签名与 `ps`/`lstart` 核对后构造
/// （字段私有 + [`VerifiedHostProvenance::new`] 强制非零），解析器拿不到它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedHostProvenance {
    host: HostIdentity,
}

impl VerifiedHostProvenance {
    /// 仅当调用方已经在**帧之外**核对过签名的 bundle/TeamID 与 `ps -p <pid> -o lstart=`
    /// 时才可构造。全零字段一律拒绝——零值不是"未知"，而是"没有证据"。
    ///
    /// **这仍只是约定，不是强制边界**（复审 Critical）：构造函数是 `pub` 的，任何调用方
    /// （包括拿到 `ProbeReport` 后回头拼一个 `HostIdentity` 的地方）都能造出一个"可信"
    /// 值。Rust 没法在不引入能力令牌 / 单独模块可见性的前提下真正强制它。本模块因此
    /// 只承诺：**它不会自己伪造可信值**。真正的强制点放在调用方——见
    /// [`VerifiedHostProvenance::new_for_verified_host`] 的文档与 Task 2 的门禁要求。
    #[cfg(test)]
    pub fn new(host: HostIdentity) -> Option<Self> {
        if host.pid == 0 || host.launch_generation == 0 || host.login_session == 0 {
            return None;
        }
        Some(Self { host })
    }

    /// 生产构造入口。调用方必须传入**帧之外**取得的宿主身份（签名 + `ps`/`lstart`），
    /// 并且必须在此前完成官方 bundle / TeamID / 构建号核对。本函数无法验证"你是否真的
    /// 核对过"，它只强制**非零**——这是本层能做到的最大程度，剩下的由调用方审查承担。
    ///
    /// 之所以把测试构造与生产构造分开：测试里的 `new` 只能在 `cfg(test)` 下存在，
    /// 生产代码里没有"随手拼一个"的入口，减少误用面。
    pub fn new_for_verified_host(host: HostIdentity) -> Option<Self> {
        if host.pid == 0 || host.launch_generation == 0 || host.login_session == 0 {
            return None;
        }
        Some(Self { host })
    }

    pub fn host(&self) -> &HostIdentity {
        &self.host
    }
}

/// 在完整多窗口快照中，解析批准键所指向的那一扇窗，并**逐一排除兄弟窗口**。
///
/// 身份缺失/为空/相撞、宿主或任一批准键不符、行集不完整、多个可能候选，一律拒绝。
/// 返回 `Matched` 仍只证明**本帧内**唯一对位，不证明跨帧稳定、可写、已受理或已送达。
///
/// **为什么必须看兄弟窗口（复审 Critical A，spec §3.2）**：此前实现只解析批准窗口，
/// 于是"同一线程同时出现在两扇窗口"时两扇都能各自 `Matched`——而 §3.2 明确要求这种情况
/// 拒绝。现在对**每一扇**窗口跑同一内核，取该窗自己的行证据：
///
/// * 批准窗必须 `Matched`；
/// * 任一兄弟窗也 `Matched` → `Ambiguous`（同一线程在两个窗口出现）；
/// * 兄弟窗为 `Ambiguous`/`Unverifiable` → `Unverifiable`（**无法排除**它也是同一线程的
///   候选；"排不掉"不等于"排除"）；
/// * 全部兄弟窗都 `OtherThread`（各自独立证明加载了别的线程）→ 才允许 `Matched`。
///
/// 禁止用兄弟窗口的身份或代际推断它加载了哪条线程——那只是"不是同一个窗口"，
/// 不是"不是同一条线程"。
/// **instrument-only：本构建不适用（规格 §8）。**
///
/// 本函数依赖窗口身份，而本构建没有可用的窗口身份属性。产品路径请用
/// [`resolve_current_target`]。
pub fn resolve_window_target(
    snapshot: &MultiWindowSnapshot,
    provenance: Option<&VerifiedHostProvenance>,
    approved: &ApprovedWindowTarget<'_>,
    lookup: impl Fn(&str) -> IndexLookup,
) -> WindowTargetBinding {
    // Critical B：帧自报的 `signature_verified` **不参与放行**——解析器一律置 false，
    // 真正的可信来源只认帧外的 `VerifiedHostProvenance`。若这里再 AND 上帧自带的那面旗，
    // 就等于允许帧文本替自己背书。
    if provenance
        .map(|p| p.host() != &snapshot.host)
        .unwrap_or(true)
        || snapshot.host != approved.host
        || approved.host.pid == 0
        || approved.host.launch_generation == 0
        || approved.host.login_session == 0
        || approved.window_identity.trim().is_empty()
        || approved.window_generation == 0
        || approved.binding_generation == 0
        || approved.workspace_key.is_empty()
        || approved.thread_id.is_empty()
        || approved.title_hash.is_empty()
        || approved.project_label_hash.is_empty()
        || snapshot.windows.is_empty()
    {
        return WindowTargetBinding::Unverifiable;
    }

    let mut identities = std::collections::BTreeSet::new();
    for window in &snapshot.windows {
        if window.window_count != snapshot.windows.len()
            || window.host != snapshot.host
            || !window.internally_consistent()
            || window.window_identity.trim().is_empty()
            || !identities.insert(window.window_identity.trim().to_string())
        {
            return WindowTargetBinding::Unverifiable;
        }
    }

    let Some(approved_window) = snapshot
        .windows
        .iter()
        .find(|window| window.window_identity.trim() == approved.window_identity.trim())
    else {
        return WindowTargetBinding::Unverifiable;
    };
    if approved_window.window_generation != approved.window_generation {
        return WindowTargetBinding::Unverifiable;
    }
    // 每一扇窗口都用**它自己的**身份、代际、标题与行跑同一纯判定内核，绝不取兄弟窗口字段。
    //
    // 注意：兄弟窗口的 `window_generation` 必须用**它自己的**值，不能复用批准窗的代际。
    // 这里问的是"这扇窗此刻加载的是哪条线程"，不是"它是不是批准窗"；若拿批准窗代际
    // 去问兄弟窗，`trusted()` 必然因代际不符而失败，所有兄弟都会退化成 `Unverifiable`，
    // 于是任何多窗场景都会被误判成"排不掉"。
    let mut approved_binding = None;
    let mut sibling_matches = 0usize;
    for window in &snapshot.windows {
        let is_approved = std::ptr::eq(window, approved_window);
        let report = ProbeReport {
            host: window.host.clone(),
            window_count: 1,
            window_generation: window.window_generation,
            window_title_hash: window.window_title_hash.clone(),
            rows: window.rows.clone(),
            groups_available: window.groups_available,
            counts: window.counts,
        };
        let window_target = ApprovedTarget {
            host: approved.host.clone(),
            window_generation: window.window_generation,
            workspace_key: approved.workspace_key,
            thread_id: approved.thread_id,
            title_hash: approved.title_hash,
            project_label_hash: approved.project_label_hash,
        };
        let binding = resolve_current_target(&report, &window_target, &lookup);
        if is_approved {
            approved_binding = Some(binding);
            continue;
        }
        match binding {
            // 兄弟窗也指向同一批准线程：§3.2 要求拒绝，不是二选一。
            TargetBinding::Matched => sibling_matches += 1,
            // 兄弟窗是别的线程：这才是可用的"排除"证据。
            TargetBinding::OtherThread => {}
            // 排不掉的兄弟窗（歧义/不可证）一律保守阻断：不能声称"唯一窗口"。
            TargetBinding::Ambiguous | TargetBinding::Unverifiable => {
                return WindowTargetBinding::Unverifiable;
            }
        }
    }

    // 兄弟否决只作用于"批准窗本来匹配"这一种情形（复审 New Breakage）。
    //
    // 若批准窗已被**独立证明**是别的线程（`OtherThread`），它就是确定的非目标，
    // 此时"有个兄弟窗匹配"不构成歧义——把它报成 `Ambiguous` 会错述失败原因
    // （真实原因：这个固定窗口已经不在目标线程上）。同理，批准窗自己
    // `Ambiguous`/`Unverifiable` 时也按它自己的状态返回，不被兄弟否决改写。
    match approved_binding.unwrap_or(TargetBinding::Unverifiable) {
        TargetBinding::Matched if sibling_matches > 0 => WindowTargetBinding::Ambiguous,
        TargetBinding::Matched => WindowTargetBinding::Matched,
        TargetBinding::OtherThread => WindowTargetBinding::OtherThread,
        TargetBinding::Ambiguous => WindowTargetBinding::Ambiguous,
        TargetBinding::Unverifiable => WindowTargetBinding::Unverifiable,
    }
}

#[cfg(test)]
mod multiwindow_binding_tests {
    use super::*;

    const WS: &str = "aaaa00001111";
    const ID_A: &str = "01234567-89ab-7def-8123-000000000001";
    const ID_B: &str = "01234567-89ab-7def-8123-000000000002";
    const H_A: &str = "8f434346648f";
    const H_B: &str = "9d74e011433f";

    fn host() -> HostIdentity {
        HostIdentity {
            pid: 2075,
            launch_generation: 1_795_773_975,
            login_session: 501,
        }
    }

    fn row(hash: &str, current: bool, group: &str) -> SidebarRow {
        SidebarRow {
            label_hash: hash.into(),
            aria_current: current,
            in_readable_region: true,
            workspace_key: WS.into(),
            node_id: "aaaaaaaaaaa1".into(),
            project_group: group.into(),
        }
    }

    fn counts(rows: usize) -> EnumerationCounts {
        EnumerationCounts {
            nodes_visited: 249,
            ax_calls: 1356,
            rows_enumerated: rows,
            structural_unknown: 0,
            budget_hit: false,
            read_failed: false,
            title_recheck_stable: true,
        }
    }

    fn window(
        identity: &str,
        generation: u64,
        title: &str,
        thread: &str,
        frontmost: bool,
        window_count: usize,
    ) -> WindowProbeReport {
        let rows = vec![row(title, true, "g0"), row(thread, false, "g0")];
        let row_count = rows.len();
        WindowProbeReport {
            host: host(),
            window_count,
            window_identity: identity.into(),
            identity_source: WindowIdentitySource::AxIdentifier,
            window_generation: generation,
            window_title_hash: title.into(),
            frontmost_observed: Some(frontmost),
            rows,
            counts: counts(row_count),
            groups_available: true,
        }
    }

    fn approved(
        identity: &'static str,
        thread: &'static str,
        title: &'static str,
        generation: u64,
    ) -> ApprovedWindowTarget<'static> {
        ApprovedWindowTarget {
            host: host(),
            window_identity: identity,
            window_generation: generation,
            binding_generation: 3,
            workspace_key: WS,
            thread_id: thread,
            title_hash: title,
            project_label_hash: WS,
        }
    }

    fn snapshot(windows: Vec<WindowProbeReport>) -> MultiWindowSnapshot {
        MultiWindowSnapshot {
            host: host(),
            signature_verified: true,
            windows,
        }
    }

    /// 帧外独立取得的宿主证据。测试里用它模拟"调用方已单独核对签名与 ps/lstart"。
    fn provenance() -> VerifiedHostProvenance {
        VerifiedHostProvenance::new_for_verified_host(host()).expect("non-zero host")
    }

    fn index(hash: &str) -> IndexLookup {
        match hash {
            H_A => IndexLookup::Unique {
                id: ID_A.into(),
                cwd: WS.into(),
            },
            H_B => IndexLookup::Unique {
                id: ID_B.into(),
                cwd: WS.into(),
            },
            _ => IndexLookup::None,
        }
    }

    fn frame_window(
        identity: &str,
        generation: u64,
        title: &str,
        thread: &str,
        frontmost: bool,
    ) -> String {
        format!(
            "WINDOW_BEGIN\nWINDOW_IDENTITY={identity}\nWINDOW_IDENTITY_SOURCE=AXIdentifier\nWINDOW_GENERATION={generation}\nWINDOW_TITLE_HASH={title}\nWINDOW_FRONTMOST_OBSERVED={}\nSIDEBAR_FOUND=1\nROWSET_COMPLETENESS=FullSidebar\nROWS_BEGIN\nROW {title} 1 {WS} g0 1 aaaaaaaaaaa1\nROW {thread} 0 {WS} g0 1 bbbbbbbbbbb2\nROWS_END\nCOUNTS nodes=249 calls=1356 rows=2 unknown=0 brake=0 failed=0 titleStable=1\nGROUPS_BEGIN\nGROUP g0 label={WS} rows=2\nGROUPS_END\nWINDOW_END\n",
            if frontmost { 1 } else { 0 }
        )
    }

    fn frame(blocks: &[String], count: usize) -> String {
        let mut out = format!(
            "HOST_PID=2075\nHOST_LAUNCH_GENERATION={}\nHOST_LOGIN_SESSION=501\n\
             ENUMERATION=Verified\nAX_WINDOWS_DROPPED=0\n",
            host().launch_generation
        );
        for block in blocks {
            out.push_str(block);
        }
        out.push_str(&format!("WINDOW_COUNT={count}\n"));
        out
    }

    #[test]
    fn swapped_window_order_and_frontmost_flags_still_bind_by_identity() {
        // 复审 Important 1：原夹具两个方向的前台标记从不翻转（A 恒 false、B 恒 true），
        // 且标题本身就能唯一定位窗口——于是"按标题选"或"按前台选"的错误实现都能通过。
        // 现在把顺序与前台标记**正交**地各自翻转，并额外验一条：批准的身份与标题不符时
        // 必须拒绝（标题成立也不能盖过身份）。
        let a_front = frame_window("win-A", 7, H_A, H_B, true);
        let b_back = frame_window("win-B", 9, H_B, H_A, false);
        // B 在前台、A 在后台，块顺序也是 B 先。
        let b_front = frame_window("win-B", 9, H_B, H_A, true);
        let a_back = frame_window("win-A", 7, H_A, H_B, false);

        for (order, first_front, second_front) in [
            ((a_front.clone(), b_back.clone()), "win-A", "win-B"),
            ((b_front.clone(), a_back.clone()), "win-B", "win-A"),
        ] {
            let snap = evaluate_multiwindow_frame_text(&frame(&[order.0, order.1], 2)).unwrap();
            // 前台标记确实换过：两帧里"谁在前台"不同。
            let frontmost_identity = snap
                .windows
                .iter()
                .find(|w| w.frontmost_observed == Some(true))
                .map(|w| w.window_identity.clone());
            assert_eq!(
                frontmost_identity.as_deref(),
                Some(first_front),
                "本帧前台应由 {first_front} 持有"
            );
            assert_eq!(
                snap.windows
                    .iter()
                    .find(|w| w.frontmost_observed == Some(false))
                    .map(|w| w.window_identity.as_str()),
                Some(second_front),
                "本帧后台应为 {second_front}"
            );
            // 无论谁在前台，两个目标都按身份各自匹配。
            assert_eq!(
                resolve_window_target(
                    &snap,
                    Some(&provenance()),
                    &approved("win-A", ID_A, H_A, 7),
                    index
                ),
                WindowTargetBinding::Matched
            );
            assert_eq!(
                resolve_window_target(
                    &snap,
                    Some(&provenance()),
                    &approved("win-B", ID_B, H_B, 9),
                    index
                ),
                WindowTargetBinding::Matched
            );
        }

        // 身份与标题不符：批准 win-A 的身份，却给出与 win-B 相同的标题 → 不得 Matched。
        let snap = evaluate_multiwindow_frame_text(&frame(&[a_front, b_back], 2)).unwrap();
        assert_eq!(
            resolve_window_target(
                &snap,
                Some(&provenance()),
                &approved("win-A", ID_A, H_B, 7),
                index
            ),
            WindowTargetBinding::Unverifiable,
            "身份对上但标题不符时必须拒绝——不得改用标题去挑另一扇窗"
        );
    }

    #[test]
    fn two_threads_with_same_title_never_claim_one_window() {
        let target = approved("win-A", ID_A, H_A, 7);
        let mut same_title = window("win-A", 7, H_A, H_A, true, 2);
        let mut other = row(H_A, false, "g0");
        other.node_id = "bbbbbbbbbbb2".into();
        same_title.rows.push(other);
        same_title.counts.rows_enumerated += 1;
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![same_title, window("win-B", 9, H_B, H_A, false, 2)]),
                Some(&provenance()),
                &target,
                index
            ),
            WindowTargetBinding::Ambiguous
        );
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![window("win-A", 7, H_A, H_B, true, 1)]),
                Some(&provenance()),
                &target,
                |_| IndexLookup::Ambiguous
            ),
            WindowTargetBinding::Ambiguous
        );
    }

    #[test]
    fn missing_empty_or_colliding_window_identity_is_unverifiable() {
        let target = approved("win-A", ID_A, H_A, 7);
        for identity in ["", "   "] {
            assert_eq!(
                resolve_window_target(
                    &snapshot(vec![window(identity, 7, H_A, H_B, true, 1)]),
                    Some(&provenance()),
                    &target,
                    index
                ),
                WindowTargetBinding::Unverifiable
            );
        }
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![
                    window("win-A", 7, H_A, H_B, true, 2),
                    window("win-A", 9, H_B, H_A, false, 2)
                ]),
                Some(&provenance()),
                &target,
                index
            ),
            WindowTargetBinding::Unverifiable
        );
    }

    #[test]
    fn duplicate_or_multiple_window_matches_are_unverifiable() {
        let target = approved("win-A", ID_A, H_A, 7);
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![
                    window("win-A", 7, H_A, H_B, true, 2),
                    window("win-A", 7, H_A, H_B, false, 2)
                ]),
                Some(&provenance()),
                &target,
                index
            ),
            WindowTargetBinding::Unverifiable
        );
        let mut a = window("win-A", 7, H_A, H_B, true, 2);
        let mut duplicate_row = a.rows[0].clone();
        duplicate_row.node_id = "bbbbbbbbbbb2".into();
        a.rows.push(duplicate_row);
        a.counts.rows_enumerated += 1;
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![a, window("win-B", 9, H_B, H_A, false, 2)]),
                Some(&provenance()),
                &target,
                index
            ),
            WindowTargetBinding::Ambiguous
        );

        // 复审 Important 1：上面的"多 current 行"用例复用了同一标题，于是删掉
        // "多个 current 行" 守卫后仍会被"同组同名"守卫兜成 Ambiguous——测不到目标守卫。
        // 这里改用**两个不同标题**的 current 行，让多 current 守卫成为唯一防线。
        let mut two_current = window("win-A", 7, H_A, H_B, true, 2);
        let mut second = two_current.rows[1].clone();
        second.label_hash = H_B.into();
        second.aria_current = true;
        second.node_id = "ccccccccccc3".into();
        two_current.rows.push(second);
        two_current.counts.rows_enumerated += 1;
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![two_current, window("win-B", 9, H_B, H_A, false, 2)]),
                Some(&provenance()),
                &target,
                index
            ),
            WindowTargetBinding::Ambiguous,
            "同一窗出现两个不同标题的 current 行时必须歧义，不能只靠同名守卫兜底"
        );
    }

    #[test]
    fn changed_process_generation_refuses_the_approved_target() {
        let target = approved("win-A", ID_A, H_A, 7);
        let mut snap = snapshot(vec![
            window("win-A", 7, H_A, H_B, true, 2),
            window("win-B", 9, H_B, H_A, false, 2),
        ]);
        assert_eq!(
            resolve_window_target(&snap, Some(&provenance()), &target, index),
            WindowTargetBinding::Matched
        );
        snap.host.launch_generation += 1;
        assert_eq!(
            resolve_window_target(&snap, Some(&provenance()), &target, index),
            WindowTargetBinding::Unverifiable
        );
        let mut stale_target = target.clone();
        stale_target.host = snap.host.clone();
        stale_target.window_generation = 8;
        let fresh = snapshot(vec![
            window("win-A", 7, H_A, H_B, true, 2),
            window("win-B", 9, H_B, H_A, false, 2),
        ]);
        assert_eq!(
            resolve_window_target(&fresh, Some(&provenance()), &stale_target, index),
            WindowTargetBinding::Unverifiable
        );
    }

    #[test]
    fn a_windows_ax_fields_cannot_vouch_for_b() {
        let snap = snapshot(vec![
            window("win-A", 7, H_A, H_B, true, 2),
            window("win-B", 9, H_B, H_A, false, 2),
        ]);
        assert_eq!(
            resolve_window_target(
                &snap,
                Some(&provenance()),
                &approved("win-B", ID_B, H_B, 9),
                index
            ),
            WindowTargetBinding::Matched,
            "基线：B 必须使用 B 自己的字段匹配"
        );
        let mut poisoned = snap.clone();
        poisoned.windows[1].window_title_hash = H_A.into();
        assert_eq!(
            resolve_window_target(
                &poisoned,
                Some(&provenance()),
                &approved("win-B", ID_B, H_B, 9),
                index
            ),
            WindowTargetBinding::Unverifiable,
            "若把 A 的标题借给 B，此断言会失败"
        );
        let mut poisoned = snap.clone();
        poisoned.windows[1].window_generation = 7;
        assert_eq!(
            resolve_window_target(
                &poisoned,
                Some(&provenance()),
                &approved("win-B", ID_B, H_B, 9),
                index
            ),
            WindowTargetBinding::Unverifiable,
            "若把 A 的代际借给 B，此断言会失败"
        );

        // 复审 Important 2：上面两例即使实现真的借了 A 的字段也仍会拒绝（借来的值同样不符
        // 批准值），因此不是决定性反例。真正的反例是——B 自己**没有可用的 current 行**，
        // 而 A 带着足以让 B 成立的证据。
        let mut b_blind = snap.clone();
        b_blind.windows[1]
            .rows
            .iter_mut()
            .for_each(|row| row.in_readable_region = false);
        assert_eq!(
            resolve_window_target(
                &b_blind,
                Some(&provenance()),
                &approved("win-B", ID_B, H_B, 9),
                index
            ),
            WindowTargetBinding::Unverifiable,
            "B 读不到可用的 current 行时，不能靠 A 的行凑出匹配"
        );
        b_blind.windows[1].rows.clear();
        b_blind.windows[1].counts.rows_enumerated = 0;
        assert_eq!(
            resolve_window_target(
                &b_blind,
                Some(&provenance()),
                &approved("win-B", ID_B, H_B, 9),
                index
            ),
            WindowTargetBinding::Unverifiable,
            "B 完全没有行时同样不得借 A 的证据"
        );
    }

    // ---------------------------------------------------------------
    // 复审 Critical A：兄弟窗口候选必须被逐一排除（spec §3.2）
    // ---------------------------------------------------------------

    /// 同一线程同时出现在两扇窗口 → 必须拒绝，不能二选一。
    #[test]
    fn same_thread_current_in_two_windows_is_ambiguous_not_a_pick() {
        // 两扇窗各自都有**唯一**的 current 行，且都指向 H_A/ID_A；身份各自不同且合法。
        // 注意：同行侧栏里不能出现两条同名行，否则先被"同组同名"守卫拦成 Ambiguous，
        // 就测不到"两个窗口各持同一线程"这条路径了。
        let snap = snapshot(vec![
            window("win-A", 7, H_A, H_B, true, 2),
            window("win-B", 9, H_A, H_B, false, 2),
        ]);
        assert_eq!(
            resolve_window_target(
                &snap,
                Some(&provenance()),
                &approved("win-A", ID_A, H_A, 7),
                index
            ),
            WindowTargetBinding::Ambiguous,
            "§3.2：同一线程在多个窗口出现时必须拒绝，而不是按身份挑一个"
        );
        assert_eq!(
            resolve_window_target(
                &snap,
                Some(&provenance()),
                &approved("win-B", ID_A, H_A, 9),
                index
            ),
            WindowTargetBinding::Ambiguous,
            "换任一窗作批准目标都不能变成 Matched"
        );
    }

    /// 兄弟窗排不掉（读不到 current / 歧义）时不得假装"唯一"。
    #[test]
    fn undecidable_sibling_blocks_the_match() {
        let target = approved("win-A", ID_A, H_A, 7);

        let mut blind_sibling = window("win-B", 9, H_B, H_B, false, 2);
        blind_sibling
            .rows
            .iter_mut()
            .for_each(|row| row.in_readable_region = false);
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![window("win-A", 7, H_A, H_B, true, 2), blind_sibling]),
                Some(&provenance()),
                &target,
                index
            ),
            WindowTargetBinding::Unverifiable,
            "兄弟窗读不到 current 时，无法排除它也是同一线程"
        );

        let mut no_current = window("win-B", 9, H_B, H_B, false, 2);
        no_current
            .rows
            .iter_mut()
            .for_each(|row| row.aria_current = false);
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![window("win-A", 7, H_A, H_B, true, 2), no_current]),
                Some(&provenance()),
                &target,
                index
            ),
            WindowTargetBinding::Unverifiable,
            "兄弟窗没有 current 行时同样排不掉"
        );
    }

    /// 复审 New Breakage：批准窗已被证明是**别的线程**时，兄弟窗匹配不得改写成 Ambiguous。
    ///
    /// 真实场景：固定在 A 上，桌面把 A 切到了别的线程，而 B 恰好加载着原目标线程。
    /// 此时 A 是确定的非目标（`OtherThread`），失败原因应如实是"这个固定窗口已不在目标
    /// 线程上"，而不是"有歧义"——后者会让人以为两个窗口都可能是目标。
    #[test]
    fn other_thread_approved_window_keeps_its_own_verdict() {
        // win-A 自己加载的是 H_B/ID_B（另一个线程），win-B 才是目标线程。
        let snap = snapshot(vec![
            window("win-A", 7, H_B, H_A, true, 2),
            window("win-B", 9, H_A, H_B, false, 2),
        ]);
        assert_eq!(
            resolve_window_target(
                &snap,
                Some(&provenance()),
                &approved("win-A", ID_A, H_A, 7),
                index
            ),
            WindowTargetBinding::OtherThread,
            "批准窗被独立证明在别的线程时，必须如实返回 OtherThread，不能被兄弟窗匹配改写成 Ambiguous"
        );
    }

    /// 兄弟窗能被独立证明在别的线程时，才允许 Matched。
    #[test]
    fn sibling_provably_on_another_thread_allows_the_match() {
        assert_eq!(
            resolve_window_target(
                &snapshot(vec![
                    window("win-A", 7, H_A, H_B, true, 2),
                    window("win-B", 9, H_B, H_A, false, 2),
                ]),
                Some(&provenance()),
                &approved("win-A", ID_A, H_A, 7),
                index
            ),
            WindowTargetBinding::Matched,
            "B 独立证明加载的是另一条线程时，A 的匹配才成立"
        );
    }

    // ---------------------------------------------------------------
    // 复审 Critical B：解析器不得自证可信
    // ---------------------------------------------------------------

    /// 帧内容完全正确、但没有帧外证据 → 不得 Matched。
    ///
    /// 这一条是本轮的关键反例：把 `signature_verified` 的判定权交回解析器，此断言立刻失败。
    #[test]
    fn perfect_frame_without_external_provenance_never_matches() {
        let a = frame_window("win-A", 7, H_A, H_B, false);
        let b = frame_window("win-B", 9, H_B, H_A, true);
        let snap = evaluate_multiwindow_frame_text(&frame(&[a, b], 2)).unwrap();
        assert!(
            !snap.signature_verified,
            "解析器必须产出未验证状态，不能替帧文本背书"
        );
        assert_eq!(
            resolve_window_target(&snap, None, &approved("win-A", ID_A, H_A, 7), index),
            WindowTargetBinding::Unverifiable,
            "没有帧外证据时，再完美的帧也不能 Matched"
        );
        // 帧外证据存在但指向另一个宿主：同样拒绝。
        let other = VerifiedHostProvenance::new_for_verified_host(HostIdentity {
            pid: host().pid + 1,
            launch_generation: host().launch_generation,
            login_session: host().login_session,
        })
        .unwrap();
        assert_eq!(
            resolve_window_target(&snap, Some(&other), &approved("win-A", ID_A, H_A, 7), index),
            WindowTargetBinding::Unverifiable,
            "帧外证据的宿主与帧不符时必须拒绝"
        );
        // 零值宿主不是"未知"，是"没有证据"。
        assert!(VerifiedHostProvenance::new_for_verified_host(HostIdentity {
            pid: 0,
            launch_generation: 0,
            login_session: 0,
        })
        .is_none());
    }

    struct FixedSnapshot {
        host: HostIdentity,
        signature_verified: bool,
        windows: Vec<WindowObservation>,
    }
    impl WindowProvider for FixedSnapshot {
        fn observe(&self, _: &HostIdentity) -> Result<Snapshot, ObservationError> {
            Ok(Snapshot {
                host: self.host.clone(),
                signature_verified: self.signature_verified,
                windows: self.windows.clone(),
            })
        }
    }

    #[test]
    fn old_single_window_path_still_refuses_two_window_snapshot() {
        let old = ProbeReport {
            host: host(),
            window_count: 2,
            window_generation: 7,
            window_title_hash: H_A.into(),
            rows: vec![row(H_A, true, "g0"), row(H_B, false, "g0")],
            groups_available: true,
            counts: counts(2),
        };
        let old_approved = ApprovedTarget {
            host: host(),
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_A,
            title_hash: H_A,
            project_label_hash: WS,
        };
        assert!(!old.internally_consistent());
        assert_eq!(
            resolve_current_target(&old, &old_approved, index),
            TargetBinding::Unverifiable
        );
        let provider = FixedSnapshot {
            host: host(),
            signature_verified: true,
            windows: vec![
                WindowObservation {
                    generation: 7,
                    loaded_thread: Some(ID_A.into()),
                },
                WindowObservation {
                    generation: 9,
                    loaded_thread: Some(ID_A.into()),
                },
            ],
        };
        assert_eq!(
            unique_loaded_window(&provider, &host(), ID_A),
            Err(ObservationError::ThreadUnknown)
        );
    }

    #[test]
    fn multiwindow_frame_keeps_each_windows_own_fields() {
        let text = frame(
            &[
                frame_window("win-A", 7, H_A, H_B, true),
                frame_window("win-B", 9, H_B, H_A, false),
            ],
            2,
        );
        let snap = evaluate_multiwindow_frame_text(&text).unwrap();
        assert_eq!(snap.windows.len(), 2);
        assert_eq!(snap.windows[0].window_identity, "win-A");
        assert_eq!(snap.windows[0].window_generation, 7);
        assert_eq!(snap.windows[0].window_title_hash, H_A);
        assert_eq!(snap.windows[1].window_identity, "win-B");
        assert_eq!(snap.windows[1].window_generation, 9);
        assert_eq!(snap.windows[1].window_title_hash, H_B);
        assert_ne!(snap.windows[0].rows, snap.windows[1].rows);
        assert_eq!(snap.windows[0].frontmost_observed, Some(true));
        assert_eq!(snap.windows[1].frontmost_observed, Some(false));
    }

    #[test]
    fn archived_v22_frames_parse_to_the_unchanged_single_window_report() {
        // 复审 Important 5：原实现用"再解析一次、然后与自身比较"来断言不变，
        // 这在解析器发生确定性回归时两端一起变、永远相等，等于没断言。
        // 这里改为与**独立算出的字面量**比较：宿主三元组、行集摘要、标题与计数都写死。
        //
        // 行集摘要 = 归档文件里全部 `ROW ` 行按原顺序以 '\n' 连接后的 SHA-256 前 16 位，
        // 由独立的 Python 计算得出（见 fix round 1 报告），不经 Rust 解析器。
        const GOLDEN: [(&str, u32, u64, u64, &str, &str); 3] = [
            // (file, pid, launch_generation, login_session, title_hash, row_digest16)
            (
                "task2-round-A1.txt",
                77903,
                1_795_590_664,
                501,
                "8f434346648f",
                "3ef0860e1347e738",
            ),
            (
                "task2-round-B.txt",
                77903,
                1_795_590_664,
                501,
                "9d74e011433f",
                "37a040d743e76ba0",
            ),
            (
                "task2-round-A2.txt",
                77903,
                1_795_590_664,
                501,
                "8f434346648f",
                "3ef0860e1347e738",
            ),
        ];
        for (name, pid, launch_generation, login_session, title_hash, row_digest) in GOLDEN {
            let text = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../docs/engineering/evidence/codex-desktop-ipc/frames-2026-11-27")
                    .join(name),
            )
            .unwrap();
            let report = evaluate_frame_text(&text, launch_generation).unwrap();
            assert_eq!(report.host.pid, pid, "{name}: 宿主 PID 必须与独立记录一致");
            assert_eq!(
                report.host.launch_generation, launch_generation,
                "{name}: 启动代际必须与独立记录一致"
            );
            assert_eq!(
                report.host.login_session, login_session,
                "{name}: 登录会话必须与独立记录一致"
            );
            assert_eq!(
                report.window_title_hash, title_hash,
                "{name}: 窗口标题哈希必须与归档时一致"
            );
            assert_eq!(report.window_count, 1, "{name}: 归档帧是单窗口");
            assert_eq!(report.rows.len(), 70, "{name}: 行数");
            assert_eq!(report.counts.rows_enumerated, 70, "{name}: 声明行数");
            assert_eq!(report.counts.nodes_visited, 249, "{name}: 节点预算实际用量");
            assert_eq!(report.counts.ax_calls, 1356, "{name}: AX 调用实际用量");
            assert!(report.internally_consistent(), "{name}: 必须自洽");
            assert_eq!(
                report.rows.iter().filter(|row| row.aria_current).count(),
                1,
                "{name}: 恰一个 current 行"
            );
            // 行集摘要：把解析后的行按同一规则重新拼回再比对，任何一行被改动都会变。
            let joined = report
                .rows
                .iter()
                .map(|row| {
                    format!(
                        "ROW {} {} {} {} {} {}",
                        row.label_hash,
                        u8::from(row.aria_current),
                        if row.workspace_key.is_empty() {
                            "-".to_string()
                        } else {
                            row.workspace_key.clone()
                        },
                        if row.project_group.is_empty() {
                            "-".to_string()
                        } else {
                            row.project_group.clone()
                        },
                        u8::from(row.in_readable_region),
                        if row.node_id.is_empty() {
                            "-".to_string()
                        } else {
                            row.node_id.clone()
                        },
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let digest = crate::services::codex_desktop_window::sha256_hex_full(joined.as_bytes());
            assert_eq!(
                &digest[..16],
                row_digest,
                "{name}: 行集摘要必须与独立计算值一致（任何行改动都会失败）"
            );
        }
    }

    #[test]
    fn multiwindow_frame_rejects_mixed_grammar_and_count_mismatch() {
        let block = frame_window("win-A", 7, H_A, H_B, true);
        let legacy = "\
HOST_PID=2075
HOST_LAUNCH_GENERATION=1795773975
HOST_LOGIN_SESSION=501
WINDOW_COUNT=1
WINDOW_TITLE_HASH=8f434346648f
SIDEBAR_FOUND=1
ROWSET_COMPLETENESS=FullSidebar
ROWS_BEGIN
ROW 8f434346648f 1 aaaa00001111 g0 1 aaaaaaaaaaa1
ROWS_END
COUNTS nodes=249 calls=1356 rows=1 unknown=0 brake=0 failed=0 titleStable=1
";
        assert!(evaluate_multiwindow_frame_text(&format!(
            "{}{}",
            frame(&[block.clone()], 1),
            legacy
        ))
        .is_err());
        assert!(evaluate_multiwindow_frame_text(&frame(&[block], 2)).is_err());
    }

    #[test]
    fn one_window_multiwindow_frame_is_accepted_without_relaxing_old_gate() {
        let text = frame(&[frame_window("win-A", 7, H_A, H_B, true)], 1);
        let snap = evaluate_multiwindow_frame_text(&text).unwrap();
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(
            resolve_window_target(
                &snap,
                Some(&provenance()),
                &approved("win-A", ID_A, H_A, 7),
                index
            ),
            WindowTargetBinding::Matched
        );
        assert!(evaluate_frame_text(&text, 7).is_err());
    }

    // ---------------------------------------------------------------
    // 复审 Critical C：降级枚举不得被当作完整列表（生产端到端）
    // ---------------------------------------------------------------

    /// 真实采样器在锁屏时打出的原样输出必须被解析器**拒绝**。
    ///
    /// 这一段是 2026-11-27 真机（PID 2075，锁屏）的实测输出，只做了原样贴入。
    /// 它同时证明三件事：采样器能正常终止；它如实报 `Incomplete`；
    /// 解析器不会把"看不见窗口"当成"没有窗口"或"窗口就这么多"。
    #[test]
    fn real_degraded_sampler_output_is_rejected_by_the_parser() {
        let live = "\
DIAG WINDOW_ATTRS_SELF_REFERENCE=1
DIAG AX_WINDOWS_SELF_REFERENCE=1
HOST_PID=2075
HOST_LAUNCH_GENERATION=1795773975
HOST_LOGIN_SESSION=501
ENUMERATION=Incomplete
AX_WINDOWS_DROPPED=1
WINDOW_COUNT=0
READ_ELAPSED_MS=52
";
        // 复审：原断言只是 `is_err()`，而这段文本里 DIAG 行排在 HOST_ 之前，
        // 会在**布局检查**就被拒，因此根本没走到枚举守卫。这里把原因钉死：
        assert_eq!(
            evaluate_multiwindow_frame_text(live).unwrap_err(),
            "AX_MULTIWINDOW_FRAME_BAD_LAYOUT",
            "以 DIAG 行开头不属于规范帧"
        );
        // 去掉 DIAG 行后是**合法布局但降级**的帧：必须由枚举守卫拒绝。
        let degraded_valid_layout = "\
HOST_PID=2075
HOST_LAUNCH_GENERATION=1795773975
HOST_LOGIN_SESSION=501
ENUMERATION=Incomplete
AX_WINDOWS_DROPPED=1
WINDOW_COUNT=0
";
        assert_eq!(
            evaluate_multiwindow_frame_text(degraded_valid_layout).unwrap_err(),
            "AX_MULTIWINDOW_FRAME_ENUMERATION_NOT_VERIFIED",
            "降级枚举必须在枚举守卫处被拒，而不是靠布局检查兜底"
        );
    }

    /// **生产者→解析器端到端**：按采样器实际打出的键顺序拼一帧，必须被接受。
    ///
    /// 复审 Important：此前"采样器输出"与"解析器输入"对不上——采样器会打
    /// `WINDOW_INDEX` / `CURRENT_ROWS` / `READ_ELAPSED_MS` / `DIAG ...`，而解析器全部拒绝，
    /// 于是真机帧只能靠手工裁剪才能喂进去。那个缺口会让"跑通了"变成"我改过文本才跑通"。
    /// 本用例的键序**照抄** `ax-multiwindow-sample.swift` 的打印顺序，不做裁剪。
    #[test]
    fn producer_wire_order_is_accepted_end_to_end() {
        let host_gen = host().launch_generation;
        // 键序**照抄** `ax-multiwindow-sample.swift::sampleWindow` 的实际打印顺序：
        // WINDOW_BEGIN → 身份 → DIAG → WINDOW_INDEX → WINDOW_GENERATION →
        // WINDOW_FRONTMOST_OBSERVED → DIAG WINDOW_INDEX → 侧栏定位 → 行遍历 →
        // WINDOW_TITLE_HASH → DIAG NODES → CURRENT_ROWS → DIAG CURRENT* →
        // DIAG TITLE_HASH_COUNT → DIAG STRUCTURAL_UNKNOWN → ROWSET_COMPLETENESS →
        // DIAG TITLE_EQ_CURRENT → ROWS → COUNTS → GROUPS → WINDOW_END。
        fn block(
            identity: &str,
            index: usize,
            generation: u64,
            frontmost: u8,
            title: &str,
            current: &str,
            other: &str,
            node_a: &str,
            node_b: &str,
        ) -> String {
            format!(
                "WINDOW_BEGIN
WINDOW_IDENTITY={identity}
WINDOW_IDENTITY_SOURCE=AXIdentifier
DIAG IDENTIFIER_PRESENT=1
DIAG ROLE=AXWindow
DIAG CHILDREN=3
WINDOW_INDEX={index}
WINDOW_GENERATION={generation}
WINDOW_FRONTMOST_OBSERVED={frontmost}
DIAG WINDOW_INDEX={index} WINDOW_TOTAL=2
WINDOW_TITLE_HASH={title}
DIAG NODES=249 CALLS=1356 CHILD_TOTAL=248 BRAKE=0 READ_FAILED=0 TITLE_UNSTABLE=0 ROWS=2
CURRENT_ROWS=1
DIAG CURRENT label={current}
DIAG TITLE_HASH_COUNT=1 currentCount=1
DIAG STRUCTURAL_UNKNOWN=0
ROWSET_COMPLETENESS=FullSidebar
DIAG TITLE_EQ_CURRENT=1
SIDEBAR_FOUND=1
ROWS_BEGIN
ROW {title} 1 {WS} g0 1 {node_a}
ROW {other} 0 {WS} g0 1 {node_b}
ROWS_END
COUNTS nodes=249 calls=1356 rows=2 unknown=0 brake=0 failed=0 titleStable=1
GROUPS_BEGIN
GROUP g0 label={WS} rows=2
GROUPS_END
WINDOW_END
"
            )
        }
        let producer_frame = format!(
            "HOST_PID=2075\nHOST_LAUNCH_GENERATION={host_gen}\nHOST_LOGIN_SESSION=501\n\
             ENUMERATION=Verified\nAX_WINDOWS_DROPPED=0\n{}WINDOW_COUNT=2\nREAD_ELAPSED_MS=96\n",
            [
                block(
                    "win-A",
                    0,
                    7,
                    1,
                    H_A,
                    H_A,
                    H_B,
                    "aaaaaaaaaaa1",
                    "bbbbbbbbbbb2"
                ),
                block(
                    "win-B",
                    1,
                    9,
                    0,
                    H_B,
                    H_B,
                    H_A,
                    "ccccccccccc3",
                    "ddddddddddd4"
                ),
            ]
            .concat()
        );
        let snap = evaluate_multiwindow_frame_text(&producer_frame)
            .expect("采样器原样输出必须能被解析（否则真机帧只能靠手工裁剪）");
        assert_eq!(snap.windows.len(), 2);
        assert_eq!(snap.windows[0].window_identity, "win-A");
        assert_eq!(snap.windows[1].window_identity, "win-B");
        assert!(
            !snap.signature_verified,
            "即便生产者帧完整，解析器也不得自证可信"
        );
        assert_eq!(
            resolve_window_target(
                &snap,
                Some(&provenance()),
                &approved("win-A", ID_A, H_A, 7),
                index
            ),
            WindowTargetBinding::Matched
        );
        assert_eq!(
            resolve_window_target(
                &snap,
                Some(&provenance()),
                &approved("win-B", ID_B, H_B, 9),
                index
            ),
            WindowTargetBinding::Matched
        );
    }

    /// 自相矛盾的侧栏证据必须拒绝。
    #[test]
    fn contradictory_sidebar_evidence_is_rejected() {
        let block =
            frame_window("win-A", 7, H_A, H_B, true).replace("SIDEBAR_FOUND=1", "SIDEBAR_FOUND=0");
        let text = format!(
            "HOST_PID=2075\nHOST_LAUNCH_GENERATION={}\nHOST_LOGIN_SESSION=501\n             ENUMERATION=Verified\nAX_WINDOWS_DROPPED=0\n{}WINDOW_COUNT=1\n",
            host().launch_generation,
            block
        );
        assert_eq!(
            evaluate_multiwindow_frame_text(&text).unwrap_err(),
            "AX_MULTIWINDOW_FRAME_CONTRADICTORY_SIDEBAR",
            "侧栏未找到却仍有行、且声称 FullSidebar，属于矛盾证据"
        );
    }

    /// 规范帧但枚举状态非 Verified → 必须拒绝。
    #[test]
    fn non_verified_enumeration_is_never_parsed() {
        let block = frame_window("win-A", 7, H_A, H_B, true);
        for (enumeration, dropped) in [("Degraded", "1"), ("Incomplete", "0"), ("Verified", "1")] {
            let text = format!(
                "HOST_PID=2075\nHOST_LAUNCH_GENERATION={}\nHOST_LOGIN_SESSION=501\n\
                 ENUMERATION={enumeration}\nAX_WINDOWS_DROPPED={dropped}\n{}WINDOW_COUNT=1\n",
                host().launch_generation,
                block
            );
            assert!(
                evaluate_multiwindow_frame_text(&text).is_err(),
                "ENUMERATION={enumeration} DROPPED={dropped} 时不得解析成可用快照"
            );
        }
        // 缺 ENUMERATION 的老帧同样不可用：没声明过完整，就不能当完整。
        let no_enumeration = format!(
            "HOST_PID=2075\nHOST_LAUNCH_GENERATION={}\nHOST_LOGIN_SESSION=501\n{}WINDOW_COUNT=1\n",
            host().launch_generation,
            block
        );
        assert!(evaluate_multiwindow_frame_text(&no_enumeration).is_err());
    }
}

// ============================================================
// 当前选中项 → 目标线程的候选绑定判定（2026-11-26 只读采样结论）
// ============================================================
//
// 依据 `docs/engineering/evidence/codex-desktop-ipc/2026-11-26-macos-window-thread-selection-evidence.md`
// 与同日复采：窗口标题哈希等于目标标题哈希、且**只有**目标侧栏行带
// `AXARIACurrent = "page"` 时，两个独立信号互相印证才判为 `Matched`。
//
// 本模块只做判定，不做遍历、不写、不聚焦。任何单项信号（仅标题、仅选中、
// 仅 mtime、仅前台）都不足以认领目标，一律降级为 `Unverifiable`。

/// 一次侧栏枚举的**数字凭证**：替代"可爱又可恨的类型令牌"。
///
/// 复审多轮教训：类型令牌（`CoverageProof`）在跨进程场景下是自欺——探针在独立进程里，
/// Rust 侧拿到的只是数据；而单纯布尔值又挡不住伪造。这里改为**与本次采样绑定的数字**：
/// 枚举起止节点数、AX 调用数、已定位侧栏的行数。判定时要求这几个数与"实际交付的行数"
/// 自洽。它不能证明探针没撒谎，但能保证**行集与声明不可分离**（少行即数对不上），
/// 并且把所有伪造点收敛到一个显式的、可被审查的单点（`ProbeReport`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumerationCounts {
    /// 枚举结束时累计访问的 AX 节点数。
    pub nodes_visited: usize,
    /// 枚举结束时累计发出的 AX 调用数。
    pub ax_calls: usize,
    /// 侧栏容器内被枚举到的行数（应 == 交付行数）。
    pub rows_enumerated: usize,
    /// 结构未知计数（任一 >0 必须降级）。
    pub structural_unknown: usize,
    /// 是否因预算/截止触顶。
    pub budget_hit: bool,
    /// 是否发生关键读取失败。
    pub read_failed: bool,
    /// 首尾标题复核是否一致。
    pub title_recheck_stable: bool,
}

/// 探针一次采样的完整自报（脱敏）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub host: HostIdentity,
    pub window_count: usize,
    pub window_generation: u64,
    pub window_title_hash: String,
    pub rows: Vec<SidebarRow>,
    pub counts: EnumerationCounts,
    /// 帧里是否带了 `GROUPS_BEGIN` 组清单块（v18 起才有）。
    /// 批准阶段靠它独立选取项目标签；旧帧为 false，不代表错误。
    pub groups_available: bool,
}

impl ProbeReport {
    /// 自报是否**内部自洽**：行数必须等于声明的枚举行数；无任何未知/失败/触顶；
    /// 标题复核稳定。
    ///
    /// 这是本模块能对跨进程数据做的最强核验：它挡不住恶意探针，但任何"少交了几行"
    /// 或"交来截断行集"的正常实现都会被拦下，从而避免上一轮那种"删几行仍称完整"的路径。
    pub fn internally_consistent(&self) -> bool {
        let c = &self.counts;
        c.rows_enumerated == self.rows.len()
            && c.structural_unknown == 0
            && !c.budget_hit
            && !c.read_failed
            && c.title_recheck_stable
            && self.window_count == 1
            && c.nodes_visited > 0
            && c.ax_calls > 0
    }
}

/// 单个侧栏行的只读采样（标签已是脱敏哈希，不含原文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRow {
    pub label_hash: String,
    /// 该行自身或**已核证的直接行容器**是否带 `AXARIACurrent == "page"`。
    pub aria_current: bool,
    pub in_readable_region: bool,
    /// 该行所属工作区键。**只应由 AX 给出**（侧栏里该行所在的「项目组」标签哈希）。
    /// 空串 = AX 未提供 → 归属不可证（这正是此前"按 cwd 唯一命中"被判定为伪证明的原因）。
    pub workspace_key: String,
    /// 该行的 `ChromeAXNodeId`（v19 起采集）。实测跨三次读取稳定且逐行互异，
    /// 是唯一可能区分"同名不同会话"的信号。空串 = 探针未取到。
    pub node_id: String,
    /// 该行所在的**项目组容器**在 AX 侧的稳定标识（组标签哈希 + 组内序号）。
    ///
    /// 与 `workspace_key` 的区别：`workspace_key` 是"这个项目叫什么"，
    /// `project_group` 是"这条行属于哪一个可区分的组实例"。同名项目出现两次时两者相同，
    /// 但组实例不同，可据此判 `Ambiguous` 而不是误认。
    pub project_group: String,
}

/// 已批准目标的身份要求。由批准流程从本地索引推导，每次新读重新核证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedTarget<'a> {
    pub host: HostIdentity,
    pub window_generation: u64,
    pub workspace_key: &'a str,
    /// 批准时的**不可变线程 ID**。判定时要求：读时索引里"该工作区内与该标题同哈希、
    /// 未归档"的唯一命中项，其 ID 必须等于此值。
    pub thread_id: &'a str,
    /// 目标标题哈希（= 批准时该线程的标题哈希）。
    pub title_hash: &'a str,
    /// 批准项目在侧栏里的组标签哈希（= 项目显示名的哈希）。
    ///
    /// 判定要求当前行 `workspace_key` 与它相等：**这是 AX 侧对"当前行属于哪个项目"的证明**，
    /// 有了它，"按 cwd 唯一命中"才不再是伪证明——AX 证归属、索引证身份，两者互不可缺。
    pub project_label_hash: &'a str,
}

/// 判定结果：**四态**，与执行单要求的"匹配当前目标 / 当前是别的线程 / 歧义 / 不可证"一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetBinding {
    /// 当前窗口的标题与唯一 current 行都对上了这条目标，**且**读时索引的唯一命中 ID
    /// 就是批准 ID。仍**不是**写门放行依据（见模块文档）。
    Matched,
    /// 当前窗口被**独立证实**加载的是另一条线程：唯一 current 行可读、标题与之一致、
    /// 且该行的哈希在本地索引里唯一命中另一条**未归档**线程（给出其脱敏 ID 前缀）。
    /// 与 `Matched` 同门槛，缺任一项即降级为 `Unverifiable`。
    OtherThread,
    /// 存在多个可成立的候选（同区同名多行、多个 current 行、索引里多个同名活线程）。
    Ambiguous,
    /// 证据不足、相互冲突，或 AX 原理上无法证明。
    Unverifiable,
}

/// 在 [`resolve_current_target`] 之上再核一条：**帧里那条 current 行**必须就是
/// 调用方准备写入的那一行。
///
/// 为什么必须单独有这一层：`resolve_current_target` 证明的是「当前行的标题/项目/索引
/// 指向批准线程」，它**不**看 `node_id`。因此"线程对、行不对"的组合能同时满足
/// 四态判定与"观测自报的行身份等于键"两个检查，却把写入地址指向另一条会话。
/// 复审 Critical 正是这个构造。
///
/// 一个线程在侧栏只应占一行；若 current 行的行身份不是预期那一行，说明要么侧栏出现了
/// 同一线程的重复行，要么调用方拿错了键——两种都必须拒绝。
pub fn resolve_current_target_for_row(
    report: &ProbeReport,
    approved: &ApprovedTarget<'_>,
    expected_row_id: &str,
    expected_group: &str,
    lookup: impl Fn(&str) -> IndexLookup,
) -> TargetBinding {
    if expected_row_id.trim().is_empty() || expected_group.trim().is_empty() {
        return TargetBinding::Unverifiable;
    }
    // 先跑四态判定：不 `Matched` 就没有继续的必要。
    let binding = resolve_current_target(report, approved, lookup);
    if binding != TargetBinding::Matched {
        return binding;
    }
    // 唯一 current 行此刻必然存在（`Matched` 的前提）。核对它的行身份。
    let current: Vec<&SidebarRow> = report.rows.iter().filter(|row| row.aria_current).collect();
    let Some(cur) = current.first() else {
        return TargetBinding::Unverifiable;
    };
    if cur.node_id.trim() != expected_row_id.trim() {
        // 线程对但行不对：不是"目标不在当前"，而是"绑定不成立"。
        return TargetBinding::Unverifiable;
    }
    // 组实例也必须对上：按下只用 `(node_id, project_group)` 寻址，帧只证行不证组
    // 会让"同一个 node_id 落在不同组实例"这种构造通过（复审第二轮 Important）。
    if cur.project_group.trim() != expected_group.trim() {
        return TargetBinding::Unverifiable;
    }
    TargetBinding::Matched
}

/// 读时索引查询结果（由调用方注入；本模块不直接读 SQLite）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexLookup {
    /// 全库（未归档）里该标题哈希**只命中一条**：给出其不可变 ID 与 cwd。
    Unique { id: String, cwd: String },
    /// 命中 0 条。
    None,
    /// 命中多条（含跨工作区同名）。
    Ambiguous,
}

fn trusted(report: &ProbeReport, approved: &ApprovedTarget<'_>) -> bool {
    report.internally_consistent()
        && report.host == approved.host
        && report.host.pid != 0
        && report.host.launch_generation != 0
        && report.host.login_session != 0
        && report.window_generation != 0
        && report.window_generation == approved.window_generation
}

/// 判定当前窗口加载的是哪条线程。
///
/// 四态门槛：
/// - 前置：`ProbeReport` 内部自洽、宿主逐字段相等、窗口代际一致；
/// - 每行工作区已知，且不出现"同哈希跨工作区"（AX 分不清 → `Unverifiable`）；
/// - 全量恰一个 current 行、可读、工作区 == 批准工作区、标题哈希 == 该行哈希；
/// - 该唯一 current 行的标签哈希交给 `lookup` 查本地索引：
///   - 唯一命中且 ID == 批准 ID 且哈希 == 批准标题 → `Matched`；
///   - 唯一命中但 ID != 批准 ID → `OtherThread`（不携带 ID）；
///   - 索引里多个同名活线程 → `Ambiguous`；
///   - 索引无命中 → `Unverifiable`（可能已归档/已改标题，不能反推）。
pub fn resolve_current_target(
    report: &ProbeReport,
    approved: &ApprovedTarget<'_>,
    lookup: impl Fn(&str) -> IndexLookup,
) -> TargetBinding {
    if !trusted(report, approved) {
        return TargetBinding::Unverifiable;
    }
    if approved.workspace_key.is_empty()
        || approved.thread_id.is_empty()
        || approved.title_hash.is_empty()
    {
        return TargetBinding::Unverifiable;
    }
    let rows = &report.rows;
    // 判定只看「当前行 + 与它同标题的行」。其它项目的无关行不参与，
    // 更不能否决整帧（复审 New Breakage：无关项目行曾整体毒化判定）。
    //
    // 归属必须来自 **AX 侧的项目组标签**，不能靠索引反推：
    // 若允许「空串 = 交给索引证明」，`WHERE cwd = 批准项目` 的唯一命中
    // 就成了对窗口归属的伪证明——窗口完全可能停在别的项目里，
    // 而批准项目内恰有一条同名记录。
    if approved.project_label_hash.is_empty() {
        return TargetBinding::Unverifiable;
    }
    let current: Vec<&SidebarRow> = rows.iter().filter(|r| r.aria_current).collect();
    if current.len() > 1 {
        return TargetBinding::Ambiguous;
    }
    let Some(cur) = current.first() else {
        return TargetBinding::Unverifiable;
    };
    if !cur.in_readable_region {
        return TargetBinding::Unverifiable;
    }
    // 当前行必须自带归属与组实例键，否则无法证明它属于哪个项目实例。
    if cur.workspace_key.is_empty() || cur.project_group.is_empty() {
        return TargetBinding::Unverifiable;
    }
    // 当前行必须属于批准项目（AX 项目标签 == 批准项目标签）。
    if cur.workspace_key != approved.project_label_hash {
        return TargetBinding::Unverifiable;
    }
    // 只对同标题行做唯一性/歧义判定。
    let same_title: Vec<&SidebarRow> = rows
        .iter()
        .filter(|r| r.label_hash == cur.label_hash)
        .collect();
    // 同一组实例内同名多条 → 无法区分。
    if same_title
        .iter()
        .filter(|r| r.project_group == cur.project_group)
        .count()
        > 1
    {
        return TargetBinding::Ambiguous;
    }
    // 同标题出现在别的组实例 / 别的项目 / 归属未知 → 标题不足以为凭，判歧义。
    // 组实例键只是**帧内**可区分、无跨帧稳定性证据，因此这里一律保守。
    // `in_readable_region == false` 的同标题行同样算：它既无法证明是同一行，
    // 也无法证明不是，静默忽略会让重复行被漏掉（复审第 5 问）。
    if same_title.iter().any(|r| {
        !r.in_readable_region
            || r.project_group.is_empty()
            || r.project_group != cur.project_group
            || r.workspace_key.is_empty()
            || r.workspace_key != approved.project_label_hash
    }) {
        return TargetBinding::Ambiguous;
    }
    if report.window_title_hash.is_empty() || report.window_title_hash != cur.label_hash {
        return TargetBinding::Unverifiable;
    }
    // 读时索引核证：唯一命中的不可变 ID 必须就是批准 ID。
    match lookup(&cur.label_hash) {
        // 全库同名多条（含跨区）→ AX 无法区分 → 歧义。
        IndexLookup::Ambiguous => TargetBinding::Ambiguous,
        IndexLookup::None => TargetBinding::Unverifiable,
        IndexLookup::Unique { id, cwd } => {
            // 注意：AX 已独立证明"当前行属于批准项目组"（见上方 workspace_key 校验），
            // 因此这里只需确认索引命中的 cwd 与该组对应。cwd 比对仍保留：防索引被配错项目。
            if cwd != approved.workspace_key {
                return TargetBinding::Unverifiable;
            }
            if id == approved.thread_id && cur.label_hash == approved.title_hash {
                TargetBinding::Matched
            } else if id != approved.thread_id {
                TargetBinding::OtherThread
            } else {
                TargetBinding::Unverifiable
            }
        }
    }
}

#[cfg(test)]
mod binding_tests {
    use super::*;

    /// 批准项目在 AX 侧的项目标签哈希（12 位小写十六进制）。
    /// 判定要求每行 `workspace_key` 与它相等 —— AX 证归属，索引证身份。
    const WS: &str = "aaaa00001111";
    const OTHER_WS: &str = "ws-other";
    const ID_A: &str = "01234567-89ab-7def-8123-000000000001";
    const ID_B: &str = "01234567-89ab-7def-8123-000000000002";
    const H_A: &str = "8f434346648f";
    const H_B: &str = "9d74e011433f";
    const H_HOME: &str = "50a412294327";

    fn host() -> HostIdentity {
        HostIdentity {
            pid: 77903,
            launch_generation: 11,
            login_session: 501,
        }
    }
    fn approved() -> ApprovedTarget<'static> {
        ApprovedTarget {
            host: host(),
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_A,
            title_hash: H_A,
            project_label_hash: WS,
        }
    }
    fn row(hash: &str, cur: bool, ws: &str) -> SidebarRow {
        SidebarRow {
            label_hash: hash.into(),
            aria_current: cur,
            in_readable_region: true,
            workspace_key: ws.into(),
            project_group: "g1".into(),
            node_id: String::new(),
        }
    }
    /// 指定项目组的分组行（用于同名跨组反例）。
    fn row_in(hash: &str, cur: bool, ws: &str, group: &str) -> SidebarRow {
        SidebarRow {
            label_hash: hash.into(),
            aria_current: cur,
            in_readable_region: true,
            workspace_key: ws.into(),
            project_group: group.into(),
            node_id: String::new(),
        }
    }
    fn counts(rows: usize) -> EnumerationCounts {
        EnumerationCounts {
            nodes_visited: 700,
            ax_calls: 1500,
            rows_enumerated: rows,
            structural_unknown: 0,
            budget_hit: false,
            read_failed: false,
            title_recheck_stable: true,
        }
    }
    fn report(title: &str, rows: Vec<SidebarRow>) -> ProbeReport {
        let n = rows.len();
        ProbeReport {
            host: host(),
            window_count: 1,
            window_generation: 7,
            window_title_hash: title.into(),
            rows,
            // 合成夹具默认不带组清单；需要时由具体用例覆盖。
            groups_available: false,
            counts: counts(n),
        }
    }
    /// 默认索引：H_A → ID_A，H_B → ID_B。
    fn idx(h: &str) -> IndexLookup {
        if h == H_A {
            IndexLookup::Unique {
                id: ID_A.into(),
                cwd: WS.into(),
            }
        } else if h == H_B {
            IndexLookup::Unique {
                id: ID_B.into(),
                cwd: WS.into(),
            }
        } else {
            IndexLookup::None
        }
    }

    // ===== 四态逐个命中 =====

    #[test]
    fn matched_when_title_current_row_and_index_id_all_agree() {
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Matched
        );
    }

    #[test]
    fn other_thread_when_index_confirms_a_different_live_id() {
        let r = report(H_B, vec![row(H_A, false, WS), row(H_B, true, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::OtherThread
        );
    }

    #[test]
    fn ambiguous_when_index_hits_multiple_live_threads() {
        let r = report(H_A, vec![row(H_A, true, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), |_| IndexLookup::Ambiguous),
            TargetBinding::Ambiguous
        );
    }

    #[test]
    fn unverifiable_when_index_has_no_hit() {
        let r = report(H_A, vec![row(H_A, true, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), |_| IndexLookup::None),
            TargetBinding::Unverifiable
        );
    }

    // ===== 四态之间不得混淆 =====

    #[test]
    fn switched_to_b_never_matches_old_a() {
        let r = report(H_B, vec![row(H_A, false, WS), row(H_B, true, WS)]);
        assert_ne!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Matched
        );
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::OtherThread
        );
    }

    // ===== 阶段一正例三连（计划 Task 1 bullet 1）=====
    //
    // 三个断言必须**各自喂新的帧**：拿手机导航去生成"桌面已切换"的证据是伪证明。
    // 第三例尤其关键：用户手动切到 B 之后，从手机发起的**新 B 请求**必须能独立求值到
    // `Matched`；这不是"旧 A 动作被复活"。

    #[test]
    fn stage_one_desktop_on_a_requesting_a_matches() {
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Matched
        );
    }

    #[test]
    fn stage_one_desktop_on_a_requesting_b_is_other_thread() {
        // 同一帧（桌面停在 A），批准目标是 B → 必须是 `OtherThread`，不是歧义、不是匹配。
        let b_target = ApprovedTarget {
            host: host(),
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_B,
            title_hash: H_B,
            project_label_hash: WS,
        };
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&r, &b_target, idx),
            TargetBinding::OtherThread
        );
    }

    #[test]
    fn stage_one_new_b_request_matches_after_user_switches_desktop_to_b() {
        // 用户**手动**把桌面切到 B（新帧），手机随后发起的是**新 B 请求**（新批准目标）。
        // 这不复用旧 A 动作：旧 A 的批准目标在同一帧上仍然是 `OtherThread`。
        let b_target = ApprovedTarget {
            host: host(),
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_B,
            title_hash: H_B,
            project_label_hash: WS,
        };
        let switched = report(H_B, vec![row(H_A, false, WS), row(H_B, true, WS)]);
        assert_eq!(
            resolve_current_target(&switched, &b_target, idx),
            TargetBinding::Matched
        );
        // 旧 A 动作在同一帧上没有复活。
        assert_eq!(
            resolve_current_target(&switched, &approved(), idx),
            TargetBinding::OtherThread
        );
    }

    #[test]
    fn stage_one_phone_navigation_cannot_stand_in_for_a_desktop_frame() {
        // 计划明令：观察前后帧与动作各自传入，不能拿手机导航生成"桌面已切换"的证据。
        // 这一条用类型本身表达：判定只接受 `ProbeReport`（AX 帧），手机请求不参与。
        // 若把 B 请求的目标当成"当前帧"，同一份 A 帧必须仍然判 `OtherThread`。
        let b_target = ApprovedTarget {
            host: host(),
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_B,
            title_hash: H_B,
            project_label_hash: WS,
        };
        let still_on_a = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&still_on_a, &b_target, idx),
            TargetBinding::OtherThread
        );
        // 反过来：请求 A 也不会因为桌面帧是 B 而"匹配"。
        let on_b = report(H_B, vec![row(H_A, false, WS), row(H_B, true, WS)]);
        assert_eq!(
            resolve_current_target(&on_b, &approved(), idx),
            TargetBinding::OtherThread
        );
    }

    #[test]
    fn stage_one_stale_a_frame_never_matches_after_the_user_left_a() {
        // 迟到的 A 帧（内容仍是"A current"但标题已是 B）不能给回 A 资格。
        // 与唯一正例的区别只在帧本身的内部一致性。
        let stale_mixed = report(H_B, vec![row(H_A, true, WS)]);
        assert_ne!(
            resolve_current_target(&stale_mixed, &approved(), idx),
            TargetBinding::Matched
        );
    }

    #[test]
    fn same_title_other_live_id_is_not_matched() {
        // 索引唯一命中但不是批准 ID（目标已改名/归档，另一条同名线程顶替）→ 不得 Matched。
        let r = report(H_A, vec![row(H_A, true, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), |_| IndexLookup::Unique {
                id: ID_B.into(),
                cwd: WS.into()
            }),
            TargetBinding::OtherThread
        );
    }

    #[test]
    fn home_and_settings_views_are_unverifiable() {
        assert_eq!(
            resolve_current_target(&report(H_HOME, vec![]), &approved(), idx),
            TargetBinding::Unverifiable
        );
        assert_eq!(
            resolve_current_target(&report(H_HOME, vec![row(H_A, true, WS)]), &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    // ===== 自洽性：少交行 / 截断 / 未知，一律不可证 =====

    #[test]
    fn row_count_must_match_declared_enumeration() {
        let mut r = report(H_A, vec![row(H_A, true, WS)]);
        r.counts.rows_enumerated = 137; // 声称 137 行却只交 1 行
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn any_unknown_budget_or_failure_degrades_to_unverifiable() {
        let base = || report(H_A, vec![row(H_A, true, WS)]);
        let mut cases = Vec::new();
        let mut r = base();
        r.counts.structural_unknown = 1;
        cases.push(r);
        let mut r = base();
        r.counts.budget_hit = true;
        cases.push(r);
        let mut r = base();
        r.counts.read_failed = true;
        cases.push(r);
        let mut r = base();
        r.counts.title_recheck_stable = false;
        cases.push(r);
        let mut r = base();
        r.window_count = 2;
        cases.push(r);
        let mut r = base();
        r.window_generation = 0;
        cases.push(r);
        let mut r = base();
        r.window_generation = 8;
        cases.push(r);
        let mut r = base();
        r.counts.nodes_visited = 0;
        cases.push(r);
        for r in cases {
            assert_eq!(
                resolve_current_target(&r, &approved(), idx),
                TargetBinding::Unverifiable
            );
        }
    }

    #[test]
    fn host_must_match_field_by_field() {
        let base = report(H_A, vec![row(H_A, true, WS)]);
        for mutate in [
            |h: &mut HostIdentity| h.pid = 1,
            |h: &mut HostIdentity| h.launch_generation = 12,
            |h: &mut HostIdentity| h.login_session = 502,
        ] {
            let mut r = base.clone();
            mutate(&mut r.host);
            assert_eq!(
                resolve_current_target(&r, &approved(), idx),
                TargetBinding::Unverifiable
            );
        }
    }

    // ===== 对抗性测试：逐条尝试绕过 Matched 的必经门槛 =====
    // 这些用例的作用是把"判定不可被绕过"从推理变成可执行断言。

    fn matched_baseline() -> TargetBinding {
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        resolve_current_target(&r, &approved(), idx)
    }

    #[test]
    fn adversarial_baseline_really_matches() {
        // 先证明基线确实能 Matched，否则后面的"改一个字段就失败"没有意义。
        assert_eq!(matched_baseline(), TargetBinding::Matched);
    }

    #[test]
    fn adversarial_current_row_with_empty_group_never_matches() {
        let mut cur = row(H_A, true, WS);
        cur.project_group = String::new();
        let r = report(H_A, vec![cur, row(H_B, false, WS)]);
        let called = std::cell::Cell::new(false);
        let got = resolve_current_target(&r, &approved(), |h| {
            called.set(true);
            idx(h)
        });
        assert_eq!(got, TargetBinding::Unverifiable);
        assert!(!called.get(), "缺组实例键时不得查询索引");
    }

    #[test]
    fn adversarial_empty_approved_project_label_never_matches() {
        // 批准项目标签为空 → 无法判定归属 → 不可证，且不查索引。
        // 此前该项目标签空值分支没有任何用例（复审 Q2 覆盖空洞）。
        let mut a = approved();
        a.project_label_hash = "";
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        let called = std::cell::Cell::new(false);
        let got = resolve_current_target(&r, &a, |h| {
            called.set(true);
            idx(h)
        });
        assert_eq!(got, TargetBinding::Unverifiable);
        assert!(!called.get(), "批准项目标签为空时不得查询索引");
    }

    #[test]
    fn adversarial_project_label_differs_from_cwd_is_still_judged_by_label() {
        // 钉住"项目标签"与"cwd"是两个不同概念：两者取值不同时，
        // 归属判定只认 project_label_hash，绝不回退去比较 cwd（复审 Q1-3）。
        let mut a = approved();
        a.project_label_hash = OTHER_WS; // 标签 ≠ cwd(WS)
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        let called = std::cell::Cell::new(false);
        let got = resolve_current_target(&r, &a, |h| {
            called.set(true);
            idx(h)
        });
        // 当前行归属是 WS，不等于批准标签 OTHER_WS → 不可证，且不查索引。
        assert_eq!(got, TargetBinding::Unverifiable);
        assert!(!called.get(), "归属不符时不得查询索引");
    }

    #[test]
    fn adversarial_current_row_in_other_project_never_matches() {
        let r = report(H_A, vec![row(H_A, true, OTHER_WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn adversarial_two_current_rows_never_match() {
        // 两条**不同标题**的 current 行：若删掉"current 唯一"这道判据，
        // 下面会一路走到索引并返回 Matched，断言即红。
        // 不能用两个同标题行——那样会被"同组同标题 >1"兜住，测不出该门槛（复审 Q2）。
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, true, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Ambiguous
        );
    }

    #[test]
    fn adversarial_no_current_row_never_matches() {
        let r = report(H_A, vec![row(H_A, false, WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn adversarial_current_row_unreadable_never_matches() {
        let mut cur = row(H_A, true, WS);
        cur.in_readable_region = false;
        let r = report(H_A, vec![cur, row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn adversarial_window_title_mismatch_never_matches() {
        // 窗口标题哈希与当前行不一致 = 切换中的混合帧。
        let r = report(H_B, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn adversarial_index_none_never_matches() {
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        let got = resolve_current_target(&r, &approved(), |_| IndexLookup::None);
        assert_eq!(got, TargetBinding::Unverifiable);
    }

    #[test]
    fn adversarial_index_ambiguous_never_matches() {
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        let got = resolve_current_target(&r, &approved(), |_| IndexLookup::Ambiguous);
        assert_eq!(got, TargetBinding::Ambiguous);
    }

    #[test]
    fn adversarial_index_cwd_mismatch_never_matches() {
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        let got = resolve_current_target(&r, &approved(), |_| IndexLookup::Unique {
            id: ID_A.into(),
            cwd: OTHER_WS.into(),
        });
        assert_eq!(got, TargetBinding::Unverifiable);
    }

    #[test]
    fn adversarial_title_hash_differs_from_approved_never_matches() {
        // 索引 ID 命中批准 ID，但当前行标题与批准标题不同 → 不可证，不是 Matched。
        let r = report(H_B, vec![row(H_B, true, WS), row(H_A, false, WS)]);
        let got = resolve_current_target(&r, &approved(), |_| IndexLookup::Unique {
            id: ID_A.into(),
            cwd: WS.into(),
        });
        assert_eq!(got, TargetBinding::Unverifiable);
    }

    #[test]
    fn adversarial_untrusted_report_never_matches() {
        // 报告内部不自洽（行数不符）→ 直接不可证，连索引都不查。
        let mut r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        r.counts.rows_enumerated = 3;
        let called = std::cell::Cell::new(false);
        let got = resolve_current_target(&r, &approved(), |h| {
            called.set(true);
            idx(h)
        });
        assert_eq!(got, TargetBinding::Unverifiable);
        assert!(!called.get(), "报告不自洽时不得查询索引");
    }

    // ===== Task 2 要求的两类合成反例：权限撤回、迟到回读 =====

    #[test]
    fn approved_host_must_be_independent_and_nonzero() {
        // 批准宿主必须来自独立记录且三字段非 0；0 值表示"未取得"，不可放行。
        for bad in [
            HostIdentity {
                pid: 0,
                launch_generation: 11,
                login_session: 501,
            },
            HostIdentity {
                pid: 77903,
                launch_generation: 0,
                login_session: 501,
            },
            HostIdentity {
                pid: 77903,
                launch_generation: 11,
                login_session: 0,
            },
        ] {
            assert!(
                approved_target_for(&bad, 1, 1, ID_A, WS, H_A, WS, 7).is_none(),
                "0 值宿主必须拒绝: {bad:?}"
            );
        }
        // 正常宿主可构造。
        assert!(approved_target_for(&host(), 1, 1, ID_A, WS, H_A, WS, 7).is_some());
    }

    #[test]
    fn approved_host_mismatch_against_report_is_unverifiable() {
        // 关键回归：批准宿主与帧内宿主不同 → 必须 Unverifiable。
        // 此前 approved_target_for 从帧里取宿主，这条恒真（自证）。
        let a = ApprovedTarget {
            host: HostIdentity {
                pid: 77903,
                launch_generation: 1700000000, // 与 report() 的 11 不同
                login_session: 501,
            },
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_A,
            title_hash: H_A,
            project_label_hash: WS,
        };
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&r, &a, idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn readability_comes_from_the_frame_not_hardcoded() {
        // v15 之前解析器把 in_readable_region 硬编码为 true，导致"当前行不可读"
        // 的防护在真机上永远触发不了（§4.17.1 第 1 项）。此用例钉住它来自帧。
        let frame =
            frame_a_full().replace(&format!("{H_A} 1 {WS} g0 1"), &format!("{H_A} 1 {WS} g0 0"));
        let r = evaluate_frame_text(&frame, 7).expect("parses");
        assert!(
            !r.rows[0].in_readable_region,
            "可读性必须来自帧的第 6 token"
        );
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn unreadable_flag_rejects_bad_values() {
        // 第 6 token 只接受 0/1，其它一律拒（fail-closed）。
        for bad in ["2", "x", "-", "true"] {
            let frame = frame_a_full().replace(
                &format!("{H_A} 1 {WS} g0 1"),
                &format!("{H_A} 1 {WS} g0 {bad}"),
            );
            assert_eq!(
                evaluate_frame_text(&frame, 7).unwrap_err(),
                "AX_BINDING_FRAME_BAD_READABLE_FLAG",
                "`{bad}` must be rejected"
            );
        }
    }

    #[test]
    fn legacy_five_token_row_is_rejected() {
        // v12/v14 的 5-token 行没有可读性，必须拒收，不能默认当可读。
        let frame =
            frame_a_full().replace(&format!("{H_A} 1 {WS} g0 1"), &format!("{H_A} 1 {WS} g0"));
        assert_eq!(
            evaluate_frame_text(&frame, 7).unwrap_err(),
            "AX_BINDING_FRAME_BAD_ROW"
        );
    }

    #[test]
    fn groups_block_is_parsed_and_does_not_affect_verdict() {
        // v18 探针输出 GROUPS 块供批准阶段独立选取项目标签。
        // 它必须被接受，且不得改变判定结果。
        let with_groups = format!(
            "{}GROUPS_BEGIN\nGROUP g0 rows=2\nGROUPS_END\n",
            frame_a_full()
        );
        let r = evaluate_frame_text(&with_groups, 7).expect("groups block must parse");
        assert!(r.internally_consistent());
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Matched
        );
        // 缺 GROUPS 块的旧帧仍可解析（向后兼容）。
        assert!(evaluate_frame_text(&frame_a_full(), 7).is_ok());
    }

    #[test]
    fn groups_available_reflects_whether_the_block_was_present() {
        // v18 帧带组清单 → true；旧帧没有 → false（不代表错误）。
        let with_block = format!(
            "{}GROUPS_BEGIN\nGROUP g0 rows=2\nGROUPS_END\n",
            frame_a_full()
        );
        assert!(
            evaluate_frame_text(&with_block, 7)
                .unwrap()
                .groups_available
        );
        assert!(
            !evaluate_frame_text(&frame_a_full(), 7)
                .unwrap()
                .groups_available
        );
        // 空块也算"带了清单"（侧栏无分组时的合法形态）。
        let empty_block = format!("{}GROUPS_BEGIN\nGROUPS_END\n", frame_a_full());
        assert!(
            evaluate_frame_text(&empty_block, 7)
                .unwrap()
                .groups_available
        );
    }

    #[test]
    fn malformed_groups_block_is_rejected() {
        let cases = [
            // 组键不是 g<数字>
            "GROUPS_BEGIN\nGROUP proj rows=2\nGROUPS_END\n",
            // 缺 rows=
            "GROUPS_BEGIN\nGROUP g0 2\nGROUPS_END\n",
            // rows= 不是数字
            "GROUPS_BEGIN\nGROUP g0 rows=x\nGROUPS_END\n",
            // 未配对
            "GROUPS_BEGIN\nGROUP g0 rows=2\n",
            // 只有 END
            "GROUPS_END\n",
        ];
        for tail in cases {
            let f = format!("{}{}", frame_a_full(), tail);
            let e = evaluate_frame_text(&f, 7).unwrap_err();
            assert!(
                e.starts_with("AX_BINDING_FRAME_BAD_GROUP_LINE")
                    || e.starts_with("AX_BINDING_FRAME_UNPAIRED_GROUPS"),
                "应为组块错误，实际 {e}（输入 {tail:?}）"
            );
        }
    }

    #[test]
    fn read_elapsed_is_accepted_as_diagnostic_only() {
        // 探针自报耗时（Task 2 要求记录）不得影响判定，也不得被当成未知键拒收。
        let with_elapsed = format!("{}READ_ELAPSED_MS=53\n", frame_a_full());
        let r = evaluate_frame_text(&with_elapsed, 7).expect("elapsed line must parse");
        assert!(r.internally_consistent());
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Matched,
            "耗时行不得改变判定结果"
        );
        // 对照：同一帧去掉耗时行结果必须相同。
        let plain = evaluate_frame_text(&frame_a_full(), 7).expect("parses");
        assert_eq!(
            resolve_current_target(&plain, &approved(), idx),
            TargetBinding::Matched
        );
    }

    #[test]
    fn permission_revoked_frame_never_matches() {
        // 权限撤回后探针只能打印 ERR=not_trusted（无帧）。判定层收到的任何
        // 不完整输入都必须降级——这里用"帧解析失败"与"解析成功但不可信"两种形态各验一次。
        // 形态 1：探针只输出 ERR 行，解析器必须拒绝（未知键）。
        let err_only = "ERR=not_trusted\n";
        let e = evaluate_frame_text(err_only, 7).unwrap_err();
        // 含 `=` 的行会走"未知键"分支（而非"未知行"），两者都 fail-closed。
        assert_eq!(e, "AX_BINDING_FRAME_UNKNOWN_KEY");

        // 形态 2：能解析但行集为空且声明 Partial（权限撤回后常见的退化形态）。
        let frame = "\
HOST_PID=77903
HOST_LAUNCH_GENERATION=11
HOST_LOGIN_SESSION=501
WINDOW_COUNT=1
WINDOW_TITLE_HASH=8f434346648f
SIDEBAR_FOUND=0
ROWSET_COMPLETENESS=Partial
ROWS_BEGIN
ROWS_END
COUNTS nodes=1 calls=1 rows=0 unknown=0 brake=0 failed=1 titleStable=1
";
        let r = evaluate_frame_text(frame, 7).expect("degenerate frame parses");
        assert!(!r.internally_consistent());
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn late_readback_from_old_generation_never_matches() {
        // 迟到回读：帧本身内容完全正确（A 的 current + 正确标题），
        // 但它来自**旧窗口代际**。判定层必须只认批准代际，不能因内容对就放行。
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        // 先确认这份内容在正确代际下确实 Matched。
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Matched
        );
        // 同一份内容、旧代际（批准 7，回读 6）→ 必须不可证。
        let stale = evaluate_frame_text(&frame_a_full(), 6).expect("parses");
        assert_eq!(
            resolve_current_target(&stale, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn late_readback_after_thread_switch_never_revives_old_target() {
        // A→B 之后迟到回来的 A 帧：内容仍是"A 是 current"，但窗口标题已变成 B。
        // 混合帧（标题与 current 行不一致）必须不可证，不得复活 A 的动作。
        let mixed = report(H_B, vec![row(H_A, true, WS), row(H_B, false, WS)]);
        assert_eq!(
            resolve_current_target(&mixed, &approved(), idx),
            TargetBinding::Unverifiable
        );
        // 反向：B 帧对 A 的批准目标必须判 OtherThread（认得出是"别的线程"）。
        let b_now = report(H_B, vec![row(H_B, true, WS), row(H_A, false, WS)]);
        assert_eq!(
            resolve_current_target(&b_now, &approved(), idx),
            TargetBinding::OtherThread
        );
    }

    #[test]
    fn same_title_row_outside_readable_region_is_ambiguous() {
        // 同标题行可读性未知 → 既不能证明是同一行、也不能证明不是 → 歧义。
        let mut unreadable = row(H_A, false, WS);
        unreadable.in_readable_region = false;
        let r = report(H_A, vec![row(H_A, true, WS), unreadable]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Ambiguous
        );
    }

    #[test]
    fn group_key_grammar_accepts_only_v14_forms() {
        // 逐形态钉死第 5 token 的语法边界（复审第 3 问）。
        let base = frame_a_full();
        let ok = |tok: &str| {
            let f = base.replace(&format!("{WS} g0"), &format!("{WS} {tok}"));
            evaluate_frame_text(&f, 7)
        };
        // 合法：`-`（空）与 g<十进制>
        assert!(ok("-").is_ok(), "`-` should be accepted");
        assert!(ok("g0").is_ok(), "`g0` should be accepted");
        assert!(ok("g9").is_ok(), "`g9` should be accepted");
        assert!(
            ok("g00").is_ok(),
            "`g00` should be accepted (仍为十进制数字)"
        );
        assert!(ok("g31").is_ok(), "`g31` should be accepted");
        // 非法（单 token）：旧占位常量、负号、非数字、大写
        for bad in ["g", "g-1", "gabc", "G0", "gx"] {
            assert_eq!(
                ok(bad).unwrap_err(),
                "AX_BINDING_FRAME_BAD_GROUP_KEY",
                "`{bad}` must be rejected as bad group key"
            );
        }
        // 含空格会先被拆成 6 个 token，走另一条拒绝路径（同样 fail-closed）。
        assert_eq!(
            ok("g 0").unwrap_err(),
            "AX_BINDING_FRAME_BAD_ROW",
            "space inside group key must not split into an accepted row"
        );
    }

    #[test]
    fn legacy_placeholder_group_key_is_rejected() {
        // 早期探针每行都写常量 `g`：那不是组实例证据，必须在解析层拒绝，
        // 否则无有效组实例的旧帧可一路走到 Matched（复审 Major 3）。
        let frame = frame_a_full().replace(&format!("{WS} g0"), &format!("{WS} g"));
        let err = evaluate_frame_text(&frame, 7).unwrap_err();
        assert_eq!(err, "AX_BINDING_FRAME_BAD_GROUP_KEY");
    }

    #[test]
    fn empty_project_label_in_frame_text_is_unverifiable() {
        // 文本级 `-` 归属：解析成空串 → 归属不可证，且**不得**去查索引。
        let frame = frame_a_full().replace(
            &format!("ROW {H_A} 1 {WS} g0 1 aaaaaaaaaaa1\n"),
            &format!("ROW {H_A} 1 - - 1 -\n"),
        );
        let r = evaluate_frame_text(&frame, 7).expect("frame should parse");
        assert!(r.rows[0].workspace_key.is_empty());
        assert!(r.rows[0].project_group.is_empty());
        let called = std::cell::Cell::new(false);
        let got = resolve_current_target(&r, &approved(), |_| {
            called.set(true);
            IndexLookup::Unique {
                id: ID_A.into(),
                cwd: WS.into(),
            }
        });
        assert_eq!(got, TargetBinding::Unverifiable);
        assert!(!called.get(), "归属不可证时不应查询索引");
    }

    #[test]
    fn same_title_in_two_distinct_group_instances_is_ambiguous() {
        // 组标签哈希相同但组实例键不同 → 标题不足以区分 → 歧义。
        let r = report(
            H_A,
            vec![row_in(H_A, true, WS, "g0"), row_in(H_A, false, WS, "g1")],
        );
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Ambiguous
        );
    }

    #[test]
    fn unrelated_foreign_workspace_row_does_not_veto_frame() {
        // 别的工作区里的**不同标题**行与本次判定无关，不得否决整帧。
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, false, OTHER_WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Matched
        );
    }
    #[test]
    fn rows_without_ax_workspace_are_unverifiable() {
        // 复审 Blocker：若允许"空串交给索引证明"，按 cwd 的唯一命中就成了对窗口归属的伪证明。
        // 正确语义：AX 必须给出项目组，否则整帧不可证。
        let mut r = evaluate_frame_text(&frame_a_full(), 7).expect("parses");
        for row in &mut r.rows {
            row.workspace_key = String::new();
        }
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn same_title_in_two_project_groups_is_ambiguous() {
        // 标题在 AX 侧出现在两个不同项目组 → AX 无法用标题区分 → 歧义。
        let r = report(
            H_A,
            vec![row_in(H_A, true, WS, "g1"), row_in(H_A, false, WS, "g2")],
        );
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Ambiguous
        );
    }

    #[test]
    fn same_title_in_other_workspace_is_ambiguous() {
        // 同标题出现在别的工作区：标题无法区分 → 歧义（不能报 Matched）。
        let r = report(H_A, vec![row(H_A, true, WS), row(H_A, false, OTHER_WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Ambiguous
        );
    }

    #[test]
    fn current_row_in_other_workspace_is_unverifiable() {
        let r = report(H_B, vec![row(H_A, false, WS), row(H_B, true, OTHER_WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn two_current_rows_are_ambiguous() {
        let r = report(H_A, vec![row(H_A, true, WS), row(H_B, true, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Ambiguous
        );
    }

    #[test]
    fn title_current_conflict_is_unverifiable() {
        let r = report(H_A, vec![row(H_A, false, WS), row(H_B, true, WS)]);
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn empty_approved_fields_are_unverifiable() {
        let r = report(H_A, vec![row(H_A, true, WS)]);
        for mutate in [
            |a: &mut ApprovedTarget<'static>| a.workspace_key = "",
            |a: &mut ApprovedTarget<'static>| a.thread_id = "",
            |a: &mut ApprovedTarget<'static>| a.title_hash = "",
        ] {
            let mut a = approved();
            mutate(&mut a);
            assert_eq!(
                resolve_current_target(&r, &a, idx),
                TargetBinding::Unverifiable
            );
        }
    }

    #[test]
    fn loose_ancestor_over_report_is_ambiguous() {
        let r = report(
            H_A,
            vec![
                row(H_A, true, WS),
                row("856e892a3eaf", true, WS),
                row("f37db12d4aff", true, WS),
            ],
        );
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Ambiguous
        );
    }

    // ===== 帧解析（evaluate_frame_text）=====

    fn frame_a_full() -> String {
        let mut s = String::new();
        s.push_str("HOST_PID=77903\nHOST_LAUNCH_GENERATION=11\nHOST_LOGIN_SESSION=501\n");
        s.push_str("WINDOW_COUNT=1\n");
        s.push_str(&format!("WINDOW_TITLE_HASH={H_A}\n"));
        s.push_str("SIDEBAR_FOUND=1\nDIAG SCAN=109\n");
        s.push_str("STRUCTURAL_UNKNOWN=0\nROWSET_COMPLETENESS=FullSidebar\n");
        s.push_str("ROWS_BEGIN\n");
        s.push_str(&format!(
            "ROW {H_A} 1 {WS} g0 1 aaaaaaaaaaa1\nROW {H_B} 0 {WS} g0 1 bbbbbbbbbbb2\n"
        ));
        s.push_str("ROWS_END\n");
        s.push_str("COUNTS nodes=700 calls=1500 rows=2 unknown=0 brake=0 failed=0 titleStable=1\n");
        s
    }

    #[test]
    fn frame_parse_accepts_well_formed_positive_frame() {
        let r = evaluate_frame_text(&frame_a_full(), 7).expect("frame should parse");
        assert_eq!(r.rows.len(), 2);
        assert!(r.internally_consistent());
        assert_eq!(r.host.pid, 77903);
        assert_eq!(r.window_title_hash, H_A);
    }

    #[test]
    fn frame_parse_rejects_missing_counts_or_host() {
        let no_counts = frame_a_full().replace(
            "COUNTS nodes=700 calls=1500 rows=2 unknown=0 brake=0 failed=0 titleStable=1\n",
            "",
        );
        assert!(evaluate_frame_text(&no_counts, 7).is_err());
        let no_host = frame_a_full().replace("HOST_PID=77903\n", "");
        assert!(evaluate_frame_text(&no_host, 7).is_err());
    }

    #[test]
    fn frame_parse_rejects_malformed_rows_and_hashes() {
        let bad = frame_a_full().replace(
            &format!("ROW {H_B} 0 {WS} g0 1 bbbbbbbbbbb2"),
            "ROW nothex 0 aaaa00001111 g0 1 -",
        );
        assert!(evaluate_frame_text(&bad, 7).is_err());
        let bad_flag = frame_a_full().replace(
            &format!("ROW {H_B} 0 {WS} g0 1 bbbbbbbbbbb2"),
            &format!("ROW {H_B} 7 aaaa00001111 g0 1 -"),
        );
        assert!(evaluate_frame_text(&bad_flag, 7).is_err());
        let unpaired = frame_a_full().replace("ROWS_END\n", "");
        assert!(evaluate_frame_text(&unpaired, 7).is_err());
    }

    #[test]
    fn frame_parse_error_messages_never_echo_content() {
        let leaky = format!("WINDOW_TITLE_HASH=secret-project-path-xyz\n");
        let err = evaluate_frame_text(&leaky, 7).unwrap_err();
        assert!(!err.contains("secret"), "错误信息不得回显原始内容: {err}");
        assert!(err.starts_with("AX_BINDING_FRAME_"), "{err}");
    }

    #[test]
    fn parsed_partial_frame_is_never_matched() {
        // 探针报 Partial（首页/设置页）→ 必须不可证。
        let mut s = frame_a_full().replace(
            "ROWSET_COMPLETENESS=FullSidebar",
            "ROWSET_COMPLETENESS=Partial",
        );
        s = s
            .replace("SIDEBAR_FOUND=1", "SIDEBAR_FOUND=0")
            .replace("DIAG SCAN=109", "DIAG SCAN=400 BRAKE=1");
        let r = evaluate_frame_text(&s, 7).expect("parses");
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn parsed_full_frame_matches_end_to_end() {
        // 端到端接线：完整帧 → 解析 → 判定 → 命中。
        let r = evaluate_frame_text(&frame_a_full(), 7).expect("parses");
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Matched
        );
    }

    #[test]
    fn approved_target_for_rejects_row_count_mismatch() {
        let mut r = evaluate_frame_text(&frame_a_full(), 7).expect("parses");
        r.counts.rows_enumerated = 137;
        assert!(approved_target_for(&host(), 0, 1, ID_A, WS, H_A, WS, 7).is_none());
    }

    #[test]
    fn approved_target_for_uses_independent_title_hash() {
        // 帧仅用于确认解析可用；批准目标不再从帧取宿主。
        assert!(evaluate_frame_text(&frame_a_full(), 7).is_ok());
        let a = approved_target_for(&host(), 1, 1, ID_A, WS, H_A, WS, 7).expect("approved");
        // 标题来自独立批准记录，而不是从 current 行反取。
        assert_eq!(a.title_hash, H_A);
        assert_eq!(a.thread_id, ID_A);
    }

    // ===== v9 真实输出形态回放（DIAG 行 + 空块 + 多窗口负帧）=====

    #[test]
    fn parses_real_v9_partial_frame_with_diag_lines() {
        // 逐字取自探针 v9 在首页的实际输出（脱敏后）。
        let frame = "\
HOST_PID=77903
HOST_LAUNCH_GENERATION=1
HOST_LOGIN_SESSION=501
WINDOW_COUNT=1
WINDOW_TITLE_HASH=50a412294327
SIDEBAR_FOUND=0
DIAG SCAN=400 BRAKE=1 READ_FAILED=0 OVER_DEADLINE=0
DIAG STRUCTURAL_UNKNOWN=0
ROWSET_COMPLETENESS=Partial
ROWS_BEGIN
ROWS_END
COUNTS nodes=0 calls=802 rows=0 unknown=0 brake=1 failed=0 titleStable=1
";
        let r = evaluate_frame_text(frame, 1).expect("v9 frame must parse");
        assert_eq!(r.rows.len(), 0);
        assert_eq!(r.counts.rows_enumerated, 0);
        // 预算触顶 → 不自洽 → 不可证
        assert!(!r.internally_consistent());
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn parses_real_v9_positive_shape_with_diag_current_lines() {
        // 模拟 v9 在会话视图的正帧形态：DIAG CURRENT 行必须被忽略而非拒帧。
        let frame = format!(
            "\
HOST_PID=77903
HOST_LAUNCH_GENERATION=11
HOST_LOGIN_SESSION=501
WINDOW_COUNT=1
WINDOW_TITLE_HASH={H_A}
SIDEBAR_FOUND=1
DIAG SCAN=109
DIAG TITLE_HASH_COUNT=1 currentCount=1
DIAG TITLE_EQ_CURRENT=1
ROWSET_COMPLETENESS=FullSidebar
ROWS_BEGIN
ROW {H_A} 1 {WS} g0 1 aaaaaaaaaaa1
ROW {H_B} 0 {WS} g0 1 bbbbbbbbbbb2
ROWS_END
DIAG CURRENT label={H_A}
COUNTS nodes=700 calls=1500 rows=2 unknown=0 brake=0 failed=0 titleStable=1
"
        );
        let r = evaluate_frame_text(&frame, 7).expect("v9 positive frame must parse");
        assert_eq!(r.rows.len(), 2);
        assert!(r.internally_consistent());
    }

    #[test]
    fn multi_window_frame_is_parsable_and_unverifiable() {
        let frame = "\
HOST_PID=77903
HOST_LAUNCH_GENERATION=11
HOST_LOGIN_SESSION=501
WINDOW_COUNT=2
WINDOW_TITLE_HASH=50a412294327
SIDEBAR_FOUND=0
ROWSET_COMPLETENESS=Partial
ROWS_BEGIN
ROWS_END
COUNTS nodes=1 calls=1 rows=0 unknown=0 brake=1 failed=0 titleStable=1
";
        let r = evaluate_frame_text(frame, 7).expect("multi-window frame parses");
        assert!(!r.internally_consistent());
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn duplicate_or_unknown_counts_tokens_are_rejected() {
        let dup = frame_a_full().replace("unknown=0 brake=0", "unknown=0 unknown=0 brake=0");
        assert!(evaluate_frame_text(&dup, 7).is_err());
        let unknown_key = frame_a_full().replace("brake=0", "bogus=0");
        assert!(evaluate_frame_text(&unknown_key, 7).is_err());
        let bad_flag = frame_a_full().replace("brake=0", "brake=2");
        assert_eq!(
            evaluate_frame_text(&bad_flag, 7).unwrap_err(),
            "AX_BINDING_FRAME_BAD_NUMBER"
        );
    }

    #[test]
    fn current_label_cross_workspace_conflict_is_unverifiable() {
        // M1：current 标签自己也跨工作区出现 → 不可证（此前只查批准标题）。
        let mut r = report(H_B, vec![row(H_A, false, WS), row(H_B, true, WS)]);
        r.rows.push(row(H_B, false, OTHER_WS));
        assert_eq!(
            resolve_current_target(&r, &approved(), idx),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn unique_hit_in_another_workspace_is_unverifiable() {
        // 复审 Blocker：窗口其实停在别的工作区的同名线程，索引唯一命中也在外区。
        // 若在 SQL 里按 cwd 预筛就会把这个事实藏起来 → 必须由判定层比对 cwd 拒绝。
        let r = report(H_A, vec![row(H_A, true, WS)]);
        let look = |_: &str| IndexLookup::Unique {
            id: ID_A.into(),
            cwd: OTHER_WS.into(),
        };
        assert_eq!(
            resolve_current_target(&r, &approved(), look),
            TargetBinding::Unverifiable
        );
    }

    #[test]
    fn sidebar_found_rejects_trailing_tokens() {
        // v9 已把 SCAN 移入 DIAG，SIDEBAR_FOUND 必须整值为 0/1。
        let f = frame_a_full().replace("SIDEBAR_FOUND=1", "SIDEBAR_FOUND=1 extra=0");
        assert_eq!(
            evaluate_frame_text(&f, 7).unwrap_err(),
            "AX_BINDING_FRAME_BAD_SIDEBAR_FLAG"
        );
    }

    #[test]
    fn four_states_all_reachable() {
        let m = resolve_current_target(&report(H_A, vec![row(H_A, true, WS)]), &approved(), idx);
        let o = resolve_current_target(
            &report(H_B, vec![row(H_A, false, WS), row(H_B, true, WS)]),
            &approved(),
            idx,
        );
        let a = resolve_current_target(&report(H_A, vec![row(H_A, true, WS)]), &approved(), |_| {
            IndexLookup::Ambiguous
        });
        let u = resolve_current_target(&report(H_HOME, vec![]), &approved(), idx);
        assert!(matches!(m, TargetBinding::Matched));
        assert!(matches!(o, TargetBinding::OtherThread));
        assert!(matches!(a, TargetBinding::Ambiguous));
        assert!(matches!(u, TargetBinding::Unverifiable));
    }
}

// ============================================================
// 探针输出解析 + 读时索引查询（供离线评估器接线）
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CountsFlags {
    brake: bool,
    failed: bool,
    title_stable: bool,
}

fn parse_counts(raw: &str) -> Result<(EnumerationCounts, CountsFlags), String> {
    // COUNTS 逐 token 严格校验：未知键、重复键、超范围值一律拒。
    const ALLOWED: &[&str] = &[
        "nodes",
        "calls",
        "rows",
        "unknown",
        "brake",
        "failed",
        "titleStable",
    ];
    let mut map: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for tok in raw.split_whitespace() {
        let Some((key, value)) = tok.split_once('=') else {
            return Err("AX_BINDING_FRAME_BAD_COUNTS".into());
        };
        if !ALLOWED.contains(&key) {
            return Err("AX_BINDING_FRAME_BAD_COUNTS".into());
        }
        let n: usize = value
            .parse()
            .map_err(|_| "AX_BINDING_FRAME_BAD_NUMBER".to_string())?;
        if map.insert(key, n).is_some() {
            return Err("AX_BINDING_FRAME_DUP_COUNTS".into());
        }
    }
    let need = |key: &str| -> Result<usize, String> {
        map.get(key)
            .copied()
            .ok_or_else(|| "AX_BINDING_FRAME_MISSING_COUNTS".to_string())
    };
    let budget_flag = need("brake")?;
    let failed_flag = need("failed")?;
    let stable_flag = need("titleStable")?;
    if budget_flag > 1 || failed_flag > 1 || stable_flag > 1 {
        return Err("AX_BINDING_FRAME_BAD_NUMBER".into());
    }
    Ok((
        EnumerationCounts {
            nodes_visited: need("nodes")?,
            ax_calls: need("calls")?,
            rows_enumerated: need("rows")?,
            structural_unknown: need("unknown")?,
            budget_hit: budget_flag == 1,
            read_failed: failed_flag == 1,
            title_recheck_stable: stable_flag == 1,
        },
        CountsFlags {
            brake: budget_flag == 1,
            failed: failed_flag == 1,
            title_stable: stable_flag == 1,
        },
    ))
}

fn parse_window_count(value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| "AX_BINDING_FRAME_BAD_NUMBER".to_string())
}

fn parse_frame_host(
    pid: Option<String>,
    launch_generation: Option<String>,
    session: Option<String>,
) -> Result<HostIdentity, String> {
    Ok(HostIdentity {
        pid: pid
            .ok_or("AX_BINDING_FRAME_MISSING_HOST")?
            .parse()
            .map_err(|_| "AX_BINDING_FRAME_BAD_NUMBER".to_string())?,
        launch_generation: launch_generation
            .ok_or("AX_BINDING_FRAME_MISSING_HOST")?
            .parse()
            .map_err(|_| "AX_BINDING_FRAME_BAD_NUMBER".to_string())?,
        login_session: session
            .ok_or("AX_BINDING_FRAME_MISSING_HOST")?
            .parse()
            .map_err(|_| "AX_BINDING_FRAME_BAD_NUMBER".to_string())?,
    })
}

/// 解析 v36 多窗口帧，返回**逐窗隔离**的快照。旧 v22 单窗帧仍只走
/// [`evaluate_frame_text`]，两种语法不得混用。
///
/// 帧格式（每行无前导空格；空行忽略）：
/// ```text
/// HOST_PID=<十进制>
/// HOST_LAUNCH_GENERATION=<十进制>
/// HOST_LOGIN_SESSION=<十进制>
/// WINDOW_BEGIN
/// WINDOW_IDENTITY=<非空单 token>
/// WINDOW_IDENTITY_SOURCE=<非空单 token>
/// WINDOW_GENERATION=<非零十进制>
/// WINDOW_TITLE_HASH=<12 位小写十六进制>
/// WINDOW_FRONTMOST_OBSERVED=<0|1>
/// SIDEBAR_FOUND=<0|1>
/// DIAG <只记录、不参与判定>
/// STRUCTURAL_UNKNOWN=<十进制>                  # 可选诊断
/// ROWSET_COMPLETENESS=<FullSidebar|Partial>
/// ROWS_BEGIN
/// ROW <labelHash> <0|1 current> <projectLabelHash|-> <g<N>|-> <0|1 readable> <nodeIdHash|->
/// ROWS_END
/// COUNTS nodes=<N> calls=<N> rows=<N> unknown=<N> brake=<0|1> failed=<0|1> titleStable=<0|1>
/// GROUPS_BEGIN                                  # 可选；出现必须成对
/// GROUP <g<N>> [label=<hash|->] rows=<N>
/// GROUPS_END
/// WINDOW_END
/// WINDOW_COUNT=<十进制>                          # 恰好一个，放在全部块之后
/// ```
/// 每个块的 `ROWS` / `GROUPS` / `COUNTS` 只属于该块；身份和标题哈希按原样保留，
/// 空身份保留为空并让绑定判为 `Unverifiable`，绝不回退到序号、标题或前台。解析只验证语法，
/// 不证明 AX 来源、身份跨帧稳定、线程归属或任何写入资格。
/// **instrument-only：本构建不适用（规格 §8）。**
///
/// 多窗帧语法只在历史取证里有意义；产品路径用 [`evaluate_frame_text`] 的单视图帧。
pub fn evaluate_multiwindow_frame_text(text: &str) -> Result<MultiWindowSnapshot, String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty()
        || !lines[0].starts_with("HOST_")
        || lines.last().copied() == Some("HOST_PID")
    {
        return Err("AX_MULTIWINDOW_FRAME_BAD_LAYOUT".into());
    }
    let mut cursor = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    let mut current_values = std::collections::BTreeMap::new();
    let host = loop {
        let line = lines
            .get(cursor)
            .ok_or("AX_MULTIWINDOW_FRAME_MISSING_HOST")?;
        cursor += 1;
        let Some((key, value)) = line.split_once('=') else {
            return Err("AX_MULTIWINDOW_FRAME_BAD_HOST".into());
        };
        if !matches!(
            key,
            "HOST_PID" | "HOST_LAUNCH_GENERATION" | "HOST_LOGIN_SESSION"
        ) || !seen.insert(key)
        {
            return Err("AX_MULTIWINDOW_FRAME_BAD_HOST".into());
        }
        current_values.insert(key, value);
        if seen.len() == 3 {
            break parse_frame_host(
                current_values.remove("HOST_PID").map(str::to_string),
                current_values
                    .remove("HOST_LAUNCH_GENERATION")
                    .map(str::to_string),
                current_values
                    .remove("HOST_LOGIN_SESSION")
                    .map(str::to_string),
            )?;
        }
    };
    let mut windows = Vec::new();
    // 复审 Critical C：枚举完整性是绑定前提，必须显式解析并拒绝非 Verified。
    // 生产者（Swift 采样器）会输出 `ENUMERATION=Verified|Degraded|Incomplete`；
    // 缺失即视为不可证——旧帧没有这一行，正说明它从未声明过自己完整。
    let mut enumeration: Option<&str> = None;
    let mut dropped: Option<&str> = None;
    while let Some(line) = lines.get(cursor) {
        if *line == "WINDOW_BEGIN" {
            cursor += 1;
            let window_count = windows.len() + 1;
            windows.push(parse_window_block(&lines, &mut cursor, host.clone())?);
            for window in &mut windows {
                window.window_count = window_count;
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("ENUMERATION=") {
            if enumeration.replace(value).is_some() {
                return Err("AX_MULTIWINDOW_FRAME_DUPLICATE_ENUMERATION".into());
            }
            cursor += 1;
            continue;
        }
        if let Some(value) = line.strip_prefix("AX_WINDOWS_DROPPED=") {
            if dropped.replace(value).is_some() {
                return Err("AX_MULTIWINDOW_FRAME_DUPLICATE_DROPPED".into());
            }
            cursor += 1;
            continue;
        }
        // 生产者结尾的耗时诊断：与判定无关，接受但不参与任何门槛。
        if let Some(value) = line.strip_prefix("READ_ELAPSED_MS=") {
            if value.parse::<u64>().is_err() {
                return Err("AX_MULTIWINDOW_FRAME_BAD_ELAPSED".into());
            }
            cursor += 1;
            continue;
        }
        if line.starts_with("WINDOW_COUNT=") {
            cursor += 1;
            break;
        }
        return Err("AX_MULTIWINDOW_FRAME_BAD_LAYOUT".into());
    }
    // 只接受"原始窗口列表完整、未丢弃任何项"的枚举。`Degraded`（含 App 自引用/非窗口项）
    // 与 `Incomplete`（回退枚举触顶）都意味着可能还有看不见的窗口，唯一性证明不成立。
    if enumeration != Some("Verified") {
        return Err("AX_MULTIWINDOW_FRAME_ENUMERATION_NOT_VERIFIED".into());
    }
    if dropped != Some("0") {
        return Err("AX_MULTIWINDOW_FRAME_WINDOWS_DROPPED".into());
    }
    let window_count = lines
        .get(cursor - 1)
        .and_then(|line| line.strip_prefix("WINDOW_COUNT="))
        .ok_or("AX_MULTIWINDOW_FRAME_MISSING_WINDOW_COUNT")?;
    let window_count = parse_window_count(window_count)?;
    // `READ_ELAPSED_MS` 允许跟在 `WINDOW_COUNT` 之后（生产者顺序），其余任何尾随内容都拒绝。
    let trailing_ok = cursor == lines.len()
        || (cursor + 1 == lines.len()
            && lines[cursor].starts_with("READ_ELAPSED_MS=")
            && lines[cursor]["READ_ELAPSED_MS=".len()..]
                .parse::<u64>()
                .is_ok());
    if !trailing_ok || window_count == 0 || window_count != windows.len() {
        return Err("AX_MULTIWINDOW_FRAME_BAD_WINDOW_COUNT".into());
    }
    Ok(MultiWindowSnapshot {
        host,
        // Critical B：解析器**永不**自证可信。帧文本是外部输入；可信与否只能由调用方
        // 在帧之外（签名 + ps/lstart）得出，并经 `VerifiedHostProvenance` 传入。
        signature_verified: false,
        windows,
    })
}

fn parse_window_block(
    lines: &[&str],
    cursor: &mut usize,
    host: HostIdentity,
) -> Result<WindowProbeReport, String> {
    let mut fields = std::collections::BTreeMap::new();
    let mut rows = Vec::new();
    let mut groups_available = false;
    let mut in_rows = false;
    let mut rows_closed = false;
    let mut in_groups = false;
    let mut groups_closed = false;
    let mut counts_raw = None;

    fn set_once<'a>(
        fields: &mut std::collections::BTreeMap<&'a str, &'a str>,
        key: &'a str,
        value: &'a str,
    ) -> Result<(), String> {
        if fields.insert(key, value).is_some() {
            Err("AX_MULTIWINDOW_FRAME_DUP_KEY".into())
        } else {
            Ok(())
        }
    }

    while let Some(line) = lines.get(*cursor) {
        if *line == "WINDOW_END" {
            *cursor += 1;
            break;
        }
        *cursor += 1;
        if *line == "ROWS_BEGIN" {
            if in_rows || rows_closed {
                return Err("AX_MULTIWINDOW_FRAME_DUP_ROWS_BLOCK".into());
            }
            in_rows = true;
            continue;
        }
        if *line == "ROWS_END" {
            if !in_rows {
                return Err("AX_MULTIWINDOW_FRAME_UNPAIRED_ROWS_END".into());
            }
            in_rows = false;
            rows_closed = true;
            continue;
        }
        if *line == "GROUPS_BEGIN" {
            if in_groups || groups_closed {
                return Err("AX_MULTIWINDOW_FRAME_DUP_GROUPS_BLOCK".into());
            }
            in_groups = true;
            continue;
        }
        if *line == "GROUPS_END" {
            if !in_groups {
                return Err("AX_MULTIWINDOW_FRAME_UNPAIRED_GROUPS_END".into());
            }
            in_groups = false;
            groups_closed = true;
            groups_available = true;
            continue;
        }
        if in_groups {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 || parts.len() > 4 || parts[0] != "GROUP" {
                return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_LINE".into());
            }
            let Some(idx) = parts[1].strip_prefix('g') else {
                return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_LINE".into());
            };
            if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
                return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_LINE".into());
            }
            let mut tail = &parts[2..];
            if tail.len() == 2 && tail[0].starts_with("label=") {
                let label = &tail[0]["label=".len()..];
                if label != "-" && !is_hash12(label) {
                    return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_LINE".into());
                }
                tail = &tail[1..];
            }
            if tail.len() != 1 {
                return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_LINE".into());
            }
            let Some(count) = tail[0].strip_prefix("rows=") else {
                return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_LINE".into());
            };
            if count.parse::<usize>().is_err() {
                return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_LINE".into());
            }
            continue;
        }
        if in_rows {
            rows.push(parse_window_row(line)?);
            continue;
        }
        if let Some(rest) = line.strip_prefix("COUNTS ") {
            if counts_raw.is_some() {
                return Err("AX_MULTIWINDOW_FRAME_DUP_COUNTS".into());
            }
            counts_raw = Some(rest);
            continue;
        }
        if line.starts_with("DIAG ") {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("AX_MULTIWINDOW_FRAME_UNKNOWN_LINE".into());
        };
        match key {
            "WINDOW_IDENTITY"
            | "WINDOW_IDENTITY_SOURCE"
            | "WINDOW_GENERATION"
            | "WINDOW_TITLE_HASH"
            | "WINDOW_FRONTMOST_OBSERVED"
            | "SIDEBAR_FOUND"
            | "ROWSET_COMPLETENESS"
            | "STRUCTURAL_UNKNOWN" => {
                set_once(&mut fields, key, value)?;
            }
            // 生产者（Swift 采样器）实际会打出的**诊断键**：与判定无关，但必须被接受，
            // 否则"采样器输出"与"解析器输入"永远对不上，真机帧只能靠手工裁剪
            // （复审 Important：CANONICAL GRAMMAR）。这里逐个列出而不是泛化前缀，
            // 保持键面可审计。
            "WINDOW_INDEX" | "CURRENT_ROWS" => {
                set_once(&mut fields, key, value)?;
            }
            _ => return Err("AX_MULTIWINDOW_FRAME_UNKNOWN_KEY".into()),
        }
    }
    if in_rows || !rows_closed || in_groups {
        return Err("AX_MULTIWINDOW_FRAME_UNPAIRED_BLOCK".into());
    }
    let need = |key: &str| {
        fields
            .get(key)
            .copied()
            .ok_or("AX_MULTIWINDOW_FRAME_MISSING")
    };
    let generation: u64 = need("WINDOW_GENERATION")?
        .parse()
        .map_err(|_| "AX_MULTIWINDOW_FRAME_BAD_NUMBER".to_string())?;
    if generation == 0 {
        return Err("AX_MULTIWINDOW_FRAME_BAD_GENERATION".into());
    }
    let title_hash = need("WINDOW_TITLE_HASH")?;
    if title_hash.is_empty() || !is_hash12(title_hash) {
        return Err("AX_MULTIWINDOW_FRAME_BAD_HASH".into());
    }
    let frontmost = match need("WINDOW_FRONTMOST_OBSERVED")? {
        "0" => Some(false),
        "1" => Some(true),
        _ => return Err("AX_MULTIWINDOW_FRAME_BAD_FRONTMOST_FLAG".into()),
    };
    if !matches!(need("SIDEBAR_FOUND")?, "0" | "1") {
        return Err("AX_MULTIWINDOW_FRAME_BAD_SIDEBAR_FLAG".into());
    }
    if !matches!(need("ROWSET_COMPLETENESS")?, "FullSidebar" | "Partial") {
        return Err("AX_MULTIWINDOW_FRAME_BAD_COMPLETENESS".into());
    }
    // 互斥证据（复审 Important）：侧栏没找到却声称 `FullSidebar`、或还带着行，
    // 是自相矛盾的帧。任何一项成立都不能当成"读全了"。
    let sidebar_found = need("SIDEBAR_FOUND")? == "1";
    let completeness = need("ROWSET_COMPLETENESS")?;
    if !sidebar_found && (completeness == "FullSidebar" || !rows.is_empty()) {
        return Err("AX_MULTIWINDOW_FRAME_CONTRADICTORY_SIDEBAR".into());
    }
    let (mut counts, _) = parse_counts(counts_raw.ok_or("AX_MULTIWINDOW_FRAME_MISSING_COUNTS")?)?;
    counts.budget_hit |= need("ROWSET_COMPLETENESS")? != "FullSidebar";
    Ok(WindowProbeReport {
        host,
        window_count: 0,
        window_identity: need("WINDOW_IDENTITY")?.to_string(),
        identity_source: WindowIdentitySource::from_observed(need("WINDOW_IDENTITY_SOURCE")?),
        window_generation: generation,
        window_title_hash: title_hash.to_string(),
        frontmost_observed: frontmost,
        rows,
        groups_available,
        counts,
    })
}

fn parse_window_row(line: &str) -> Result<SidebarRow, String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() != 7 || parts[0] != "ROW" {
        return Err("AX_MULTIWINDOW_FRAME_BAD_ROW".into());
    }
    if !is_hash12(parts[1]) {
        return Err("AX_MULTIWINDOW_FRAME_BAD_HASH".into());
    }
    let aria_current = match parts[2] {
        "1" => true,
        "0" => false,
        _ => return Err("AX_MULTIWINDOW_FRAME_BAD_ROW_FLAG".into()),
    };
    let workspace_key = if parts[3] == "-" {
        String::new()
    } else {
        if !is_hash12(parts[3]) {
            return Err("AX_MULTIWINDOW_FRAME_BAD_HASH".into());
        }
        parts[3].to_string()
    };
    let project_group = if parts[4] == "-" {
        String::new()
    } else {
        let Some(idx) = parts[4].strip_prefix('g') else {
            return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_KEY".into());
        };
        if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
            return Err("AX_MULTIWINDOW_FRAME_BAD_GROUP_KEY".into());
        }
        parts[4].to_string()
    };
    let in_readable_region = match parts[5] {
        "1" => true,
        "0" => false,
        _ => return Err("AX_MULTIWINDOW_FRAME_BAD_READABLE_FLAG".into()),
    };
    let node_id = if parts[6] == "-" {
        String::new()
    } else {
        if !is_hash12(parts[6]) {
            return Err("AX_MULTIWINDOW_FRAME_BAD_NODE_ID".into());
        }
        parts[6].to_string()
    };
    Ok(SidebarRow {
        label_hash: parts[1].to_string(),
        aria_current,
        in_readable_region,
        workspace_key,
        project_group,
        node_id,
    })
}

/// 解析探针 v7 的脱敏输出文本，构造 [`ProbeReport`]。
///
/// 只接受 v22 单窗口语法：`KEY=value` 行、`ROW <hash> <0|1>` 行与 `COUNTS ...` 行；
/// 任何必需字段缺失、行数不自洽、或出现未知标记都返回 `Err`（调用方据此判
/// `Unverifiable`）。多窗口帧必须走 [`evaluate_multiwindow_frame_text`]，不得混喂。
pub fn evaluate_frame_text(text: &str, window_generation: u64) -> Result<ProbeReport, String> {
    // 严格解析；任何缺失/重复/未知/格式错误都返回**固定错误码**，绝不把原始行回显
    // （复审 Major：错误信息会把正文或路径带出）。
    let mut window_count: Option<String> = None;
    let mut title_hash: Option<String> = None;
    let mut host_pid: Option<String> = None;
    let mut host_gen: Option<String> = None;
    let mut host_session: Option<String> = None;
    let mut sidebar_found: Option<bool> = None;
    let mut completeness: Option<String> = None;
    let mut counts_raw: Option<String> = None;
    let mut rows: Vec<SidebarRow> = Vec::new();
    let mut in_rows = false;
    let mut rows_closed = false;
    // GROUPS 块：探针输出本项目组清单，仅供**批准阶段**独立选取项目标签。
    // 解析时严格校验其形态（防止畸形块混过），但不参与判定。
    let mut in_groups = false;
    let mut groups_closed = false;

    fn set_once(slot: &mut Option<String>, v: &str) -> Result<(), String> {
        if slot.is_some() {
            return Err("AX_BINDING_FRAME_DUP_KEY".into());
        }
        *slot = Some(v.to_string());
        Ok(())
    }

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 诊断行统一 DIAG 前缀：不参与判定，也不算未知键。
        if line.starts_with("DIAG ") {
            continue;
        }
        if line == "ROWS_BEGIN" {
            if in_rows || rows_closed {
                return Err("AX_BINDING_FRAME_DUP_ROWS_BLOCK".into());
            }
            in_rows = true;
            continue;
        }
        if line == "GROUPS_BEGIN" {
            if in_groups || groups_closed {
                return Err("AX_BINDING_FRAME_DUP_GROUPS_BLOCK".into());
            }
            in_groups = true;
            continue;
        }
        if line == "GROUPS_END" {
            if !in_groups {
                return Err("AX_BINDING_FRAME_UNPAIRED_GROUPS_END".into());
            }
            in_groups = false;
            groups_closed = true;
            continue;
        }
        if in_groups {
            // v22 语法：GROUP <g<十进制>> [label=<12位小写十六进制|->] rows=<十进制>
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 || parts.len() > 4 || parts[0] != "GROUP" {
                return Err("AX_BINDING_FRAME_BAD_GROUP_LINE".into());
            }
            let Some(idx) = parts[1].strip_prefix('g') else {
                return Err("AX_BINDING_FRAME_BAD_GROUP_LINE".into());
            };
            if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
                return Err("AX_BINDING_FRAME_BAD_GROUP_LINE".into());
            }
            // 可选 label=
            let mut tail = &parts[2..];
            if tail.len() == 2 && tail[0].starts_with("label=") {
                let lab = &tail[0]["label=".len()..];
                if lab != "-" && !is_hash12(lab) {
                    return Err("AX_BINDING_FRAME_BAD_GROUP_LINE".into());
                }
                tail = &tail[1..];
            }
            if tail.len() != 1 {
                return Err("AX_BINDING_FRAME_BAD_GROUP_LINE".into());
            }
            let Some(count) = tail[0].strip_prefix("rows=") else {
                return Err("AX_BINDING_FRAME_BAD_GROUP_LINE".into());
            };
            if count.parse::<usize>().is_err() {
                return Err("AX_BINDING_FRAME_BAD_GROUP_LINE".into());
            }
            continue;
        }
        if line == "ROWS_END" {
            if !in_rows {
                return Err("AX_BINDING_FRAME_UNPAIRED_ROWS_END".into());
            }
            in_rows = false;
            rows_closed = true;
            continue;
        }
        if in_rows {
            // v19 语法：ROW <labelHash> <0|1 current> <projectLabelHash|-> <groupKey|-> <0|1 readable> <nodeId|->
            // 第 6 token = 行级可读性（v15）；第 7 token = ChromeAXNodeId（v19，实测逐行稳定互异）。
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 7 || parts[0] != "ROW" {
                return Err("AX_BINDING_FRAME_BAD_ROW".into());
            }
            if !is_hash12(parts[1]) {
                return Err("AX_BINDING_FRAME_BAD_HASH".into());
            }
            let cur = match parts[2] {
                "1" => true,
                "0" => false,
                _ => return Err("AX_BINDING_FRAME_BAD_ROW_FLAG".into()),
            };
            let ws = if parts[3] == "-" {
                String::new()
            } else {
                parts[3].to_string()
            };
            if !ws.is_empty() && !is_hash12(&ws) {
                return Err("AX_BINDING_FRAME_BAD_HASH".into());
            }
            // 组实例键只接受 v14 形态 `g<十进制序数>` 或 `-`。
            // 早期探针把每行都写成常量 `g`（占位），那不是组实例证据：
            // 若放行，无有效组实例的旧帧可达 Matched（复审 Major 3）。
            let group = if parts[4] == "-" {
                String::new()
            } else {
                let Some(idx) = parts[4].strip_prefix('g') else {
                    return Err("AX_BINDING_FRAME_BAD_GROUP_KEY".into());
                };
                if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("AX_BINDING_FRAME_BAD_GROUP_KEY".into());
                }
                parts[4].to_string()
            };
            let readable = match parts[5] {
                "1" => true,
                "0" => false,
                _ => return Err("AX_BINDING_FRAME_BAD_READABLE_FLAG".into()),
            };
            let node_id = if parts[6] == "-" {
                String::new()
            } else {
                if !is_hash12(parts[6]) {
                    return Err("AX_BINDING_FRAME_BAD_NODE_ID".into());
                }
                parts[6].to_string()
            };
            rows.push(SidebarRow {
                label_hash: parts[1].to_string(),
                aria_current: cur,
                in_readable_region: readable,
                workspace_key: ws,
                project_group: group,
                node_id,
            });
            continue;
        }
        // COUNTS 是空格分隔的键值串，必须先单独识别，否则会被 split_once('=')
        // 切成 key="COUNTS nodes" 而误判为未知键。
        if let Some(rest) = line.strip_prefix("COUNTS ") {
            set_once(&mut counts_raw, rest)?;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("AX_BINDING_FRAME_UNKNOWN_LINE".into());
        };
        match key {
            "WINDOW_COUNT" => set_once(&mut window_count, value)?,
            "WINDOW_TITLE_HASH" => set_once(&mut title_hash, value)?,
            // 实际形态是 `1 SCAN=400` / `0 SCAN=...`：取首个 token。
            "SIDEBAR_FOUND" => {
                if sidebar_found.is_some() {
                    return Err("AX_BINDING_FRAME_DUP_KEY".into());
                }
                // 整值必须是 0 或 1（诊断信息走 DIAG 行，不搭在这里）。
                match value {
                    "1" => sidebar_found = Some(true),
                    "0" => sidebar_found = Some(false),
                    _ => return Err("AX_BINDING_FRAME_BAD_SIDEBAR_FLAG".into()),
                }
            }
            "HOST_PID" => set_once(&mut host_pid, value)?,
            "HOST_LAUNCH_GENERATION" => set_once(&mut host_gen, value)?,
            "HOST_LOGIN_SESSION" => set_once(&mut host_session, value)?,
            "ROWSET_COMPLETENESS" => set_once(&mut completeness, value)?,
            // 已观察到的诊断字段：允许存在但不参与判定。
            "SCAN"
            | "NODES"
            | "STRUCTURAL_UNKNOWN"
            | "ELAPSED_MS"
            | "TITLE_HASH_COUNT"
            | "TITLE_EQ_CURRENT"
            | "CURRENT_ROWS"
            | "A_HASH"
            | "B_HASH"
            | "BRAKE"
            | "READ_FAILED"
            | "OVER_DEADLINE"
            | "WINDOW_FINGERPRINT"
            | "TITLE_UNSTABLE"
            | "TITLE_HASH_ROW_PRESENT"
            // 探针自报的读取耗时（Task 2 要求记录）。仅诊断用，不参与判定。
            | "READ_ELAPSED_MS" => {}
            _ => return Err("AX_BINDING_FRAME_UNKNOWN_KEY".into()),
        }
    }
    if in_rows || !rows_closed {
        return Err("AX_BINDING_FRAME_UNPAIRED_ROWS".into());
    }
    // GROUPS 块自 v18 起才有，旧帧允许缺失；但一旦出现就必须成对。
    if in_groups {
        return Err("AX_BINDING_FRAME_UNPAIRED_GROUPS".into());
    }
    let groups_available = groups_closed;
    let parse_or = |v: Option<String>| -> Result<usize, String> {
        v.ok_or_else(|| "AX_BINDING_FRAME_MISSING".to_string())?
            .parse()
            .map_err(|_| "AX_BINDING_FRAME_BAD_NUMBER".to_string())
    };
    let host_pid = host_pid.ok_or("AX_BINDING_FRAME_MISSING_HOST")?;
    let host_gen = host_gen.ok_or("AX_BINDING_FRAME_MISSING_HOST")?;
    let host_session = host_session.ok_or("AX_BINDING_FRAME_MISSING_HOST")?;
    let window_count = parse_or(window_count)?;
    let title_hash = title_hash.ok_or("AX_BINDING_FRAME_MISSING")?;
    if !title_hash.is_empty() && !is_hash12(&title_hash) {
        return Err("AX_BINDING_FRAME_BAD_HASH".into());
    }
    let counts_raw = counts_raw.ok_or("AX_BINDING_FRAME_MISSING_COUNTS")?;
    let declared_full =
        completeness.as_deref() == Some("FullSidebar") && sidebar_found.unwrap_or(false);
    let (mut counts, _) = parse_counts(&counts_raw)?;
    // 探针声明不完整 == 触顶（两者任一都降级）。
    counts.budget_hit |= !declared_full;
    let host = parse_frame_host(Some(host_pid), Some(host_gen), Some(host_session))?;

    Ok(ProbeReport {
        host,
        window_count,
        window_generation,
        window_title_hash: title_hash,
        rows,
        groups_available,
        counts,
    })
}

/// 是否为 12 位小写十六进制哈希。
fn is_hash12(s: &str) -> bool {
    s.len() == 12
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// 用**独立记录**的宿主身份构造批准目标（不是从被观测帧里取）。
///
/// 复审教训：此前该函数取 `report.host.clone()` 当批准宿主，使 `trusted()` 的宿主比对
/// 在与帧同源的调用路径上恒真——等于自证。批准宿主必须来自调用方**独立持有**的记录
/// （例如批准当时由 `ps`/`lstart` 取得并落盘的及早快照），否则"同一 App 实例"这一条
/// 门槛就没有判别力。
#[allow(clippy::too_many_arguments)] // 批准目标是独立证据的逐字段快照，合并成结构体会让调用方误以为可省略某项
pub fn approved_target_for<'a>(
    approved_host: &HostIdentity,
    report_row_count: usize,
    report_declared_rows: usize,
    thread_id: &'a str,
    workspace_key: &'a str,
    approved_title_hash: &'a str,
    project_label_hash: &'a str,
    window_generation: u64,
) -> Option<ApprovedTarget<'a>> {
    if thread_id.is_empty()
        || workspace_key.is_empty()
        || approved_title_hash.is_empty()
        || project_label_hash.is_empty()
    {
        return None;
    }
    // 批准宿主本身必须是可核验的实数（0 值表示未取得，不可放行）。
    if approved_host.pid == 0
        || approved_host.launch_generation == 0
        || approved_host.login_session == 0
    {
        return None;
    }
    // 交付行数必须与自报枚举数一致，否则不构造（少交行 = 不可信）。
    if report_row_count != report_declared_rows {
        return None;
    }
    Some(ApprovedTarget {
        host: approved_host.clone(),
        window_generation,
        workspace_key,
        thread_id,
        title_hash: approved_title_hash,
        project_label_hash,
    })
}

/// 读时索引查询：某工作区（cwd）内、未归档线程中，标题哈希唯一命中的线程 ID。
///
/// 只读打开 `~/.codex/state_*.sqlite`（`mode=ro`）。查询失败、表缺失、或同哈希
/// 活线程 >1 都返回 `Ambiguous`/`None`，绝不给"猜一个"。
pub fn lookup_thread_by_title(cwd: &str, title_hash: &str) -> Result<IndexLookup, String> {
    // 按 cwd 限域是**安全的**，前提是调用方已由 AX 独立证明"当前行属于该项目的组"
    // （见 `resolve_current_target` 的 workspace_key 校验）。
    // 早期版本在 AX 未证归属时用 cwd 限域，被判定为伪证明；现在归属已由 AX 给出，
    // 索引只需回答"这个项目里这条标题是谁"。
    let home = dirs::home_dir().ok_or_else(|| "AX_INDEX_NO_HOME".to_string())?;
    let codex = home.join(".codex");
    let state =
        crate::monitor::codex_thread_parser::CodexThreadRoots::from_codex_root(&codex).state_db;
    let conn = crate::monitor::sqlite::open_readonly_with_timeout(&state)
        .ok_or_else(|| "AX_INDEX_UNREADABLE".to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, COALESCE(title,''), COALESCE(cwd,'') \
             FROM threads WHERE archived = 0 AND cwd = ?1",
        )
        .map_err(|_| "AX_INDEX_PREPARE_FAILED".to_string())?;
    let mapped = stmt
        .query_map([cwd], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| "AX_INDEX_QUERY_FAILED".to_string())?;
    let mut hits: Vec<(String, String)> = Vec::new();
    for r in mapped {
        let (id, title, row_cwd) = r.map_err(|_| "AX_INDEX_ROW_FAILED".to_string())?;
        if sha256_prefix12(&title) == title_hash {
            hits.push((id, row_cwd));
        }
    }
    // 同一 ID 重复出现不算多条。
    hits.sort();
    hits.dedup_by(|a, b| a.0 == b.0);
    Ok(match hits.len() {
        0 => IndexLookup::None,
        1 => {
            let (id, row_cwd) = hits.remove(0);
            IndexLookup::Unique { id, cwd: row_cwd }
        }
        _ => IndexLookup::Ambiguous,
    })
}

pub fn sha256_prefix12(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(s.as_bytes());
    d.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(12)
        .collect()
}

/// 完整 SHA-256 小写十六进制。仅用于**离线回归夹具**把归档行集钉成字面量摘要，
/// 不参与任何判定；生产路径不要用它当安全边界的替代。
#[cfg(test)]
fn sha256_hex_full(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ============================================================
// 阶段二：隔离导航（Task 6）
// ============================================================

/// 目标会话行**不是**当前会话时的独立证明。
///
/// 阶段一的 `resolve_current_target` 只回答"当前加载的是不是目标"，回答不了
/// "目标那一行确实存在、唯一、可操作"。阶段二必须先取得后者，才能谈"切过去"。
///
/// 这是**只读**证明：它不点击、不改变焦点、不产生任何桌面副作用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRowProof {
    /// 目标行在侧栏里唯一、可读，且确实**不是**当前行。
    ///
    /// 注意：本构建的行采样里**没有** enabled/disabled 字段，所以这里无法证明"可操作"。
    /// 该缺口如实保留——不编造一个永远不产生的 `RowDisabled` 变体来假装覆盖了它。
    Navigable {
        /// 目标行的 `ChromeAXNodeId` 摘要。
        row_identity: String,
        /// 目标行所属项目组实例键。
        project_group: String,
    },
    /// 目标已经是当前会话——不需要导航（阶段一直接处理）。
    AlreadyCurrent,
    /// 目标行不存在 / 不唯一 / 不可读 / 已禁用。
    NotNavigable(TargetRowRefusal),
}

/// 为什么目标行不能被导航到。每一项都对应一个具体的、已检查过的失败面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRowRefusal {
    /// 侧栏里找不到与批准标题哈希相符的可读行。
    RowAbsent,
    /// 同标题多行，无法唯一定向。
    RowAmbiguous,
    /// 目标行的项目组实例无法确认（同名不同组）。
    GroupUnproven,
    /// 帧本身不可信（枚举不完整、读取失败、宿主不符等）。
    FrameUntrusted,
    /// 目标行身份为空。
    RowIdentityMissing,
}

/// 从一帧只读采样里证明"目标行可唯一定向且当前不是它"。
///
/// 判据顺序是刻意的：先证明帧可信，再证明目标行唯一，最后才区分"已经是当前"与
/// "需要导航"。任何一步不成立都返回拒绝，不会退化成"试试看"。
pub fn prove_target_row(
    report: &ProbeReport,
    approved: &ApprovedTarget<'_>,
    target_row_id: &str,
) -> TargetRowProof {
    // 复用 `trusted()` 的非零门槛：全零宿主曾经能通过"只要求相等"的比较。
    if !report.internally_consistent()
        || report.host != approved.host
        || report.host.pid == 0
        || report.host.launch_generation == 0
        || report.host.login_session == 0
        || report.window_generation == 0
        || report.window_generation != approved.window_generation
    {
        return TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted);
    }
    if approved.title_hash.is_empty() || approved.project_label_hash.is_empty() {
        return TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted);
    }
    if target_row_id.trim().is_empty() {
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowIdentityMissing);
    }
    // **先按标题取全部同名行**，再看其中有几行落在批准项目里。
    //
    // 复审 Critical：早期版本把"项目标签"塞进同一个 filter，于是"同名、同组、
    // 但项目标签不同"的第二行会被静默排除，后面的组检查也就看不见它——
    // 相当于挑了一个候选当唯一。这里必须先把同名行全拿出来。
    let same_title: Vec<&SidebarRow> = report
        .rows
        .iter()
        .filter(|row| row.label_hash == approved.title_hash)
        .collect();
    if same_title.is_empty() {
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowAbsent);
    }
    // 任一同名行不在可读区 → 既不能证明是同一行、也不能证明不是。
    if same_title.iter().any(|row| !row.in_readable_region) {
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous);
    }
    // 任一同名行的归属未知 → 无法排除它在批准项目里。
    if same_title
        .iter()
        .any(|row| row.workspace_key.trim().is_empty())
    {
        return TargetRowProof::NotNavigable(TargetRowRefusal::GroupUnproven);
    }
    let candidates: Vec<&SidebarRow> = same_title
        .iter()
        .copied()
        .filter(|row| row.workspace_key == approved.project_label_hash)
        .collect();
    if candidates.is_empty() {
        // 同名行都在别的项目里：不是"目标行可定向"。
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowAbsent);
    }
    if candidates.len() > 1 {
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous);
    }
    // 同名行出现在两个不同的组实例里 → 组证据不足（即使只有一个落在批准项目内）。
    if same_title
        .iter()
        .any(|row| row.project_group.trim().is_empty())
    {
        return TargetRowProof::NotNavigable(TargetRowRefusal::GroupUnproven);
    }
    // **按下地址必须全帧唯一**（复审第二轮 Critical）。
    //
    // `press_target_row` 只用 `(row_identity, project_group)` 寻址。如果**任何**另一行
    // ——哪怕标题完全不同——复用了这两个值，按下就不再唯一指向一条会话。
    // 只检查"同名行之间唯一"是不够的。
    let target_node = candidates[0].node_id.trim();
    let target_group = candidates[0].project_group.trim();
    let address_dupes = report
        .rows
        .iter()
        .filter(|row| row.node_id.trim() == target_node && row.project_group.trim() == target_group)
        .count();
    if address_dupes != 1 {
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous);
    }
    // **归属冲突**：同名行之间项目标签不一致，说明"这条标题属于哪个项目"本身不可证。
    // 只在批准项目内挑一个候选当唯一，等于无视另一条同名行的存在。
    if same_title
        .iter()
        .any(|row| row.workspace_key != approved.project_label_hash)
    {
        return TargetRowProof::NotNavigable(TargetRowRefusal::GroupUnproven);
    }
    let target = candidates[0];
    if !target.in_readable_region {
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowAbsent);
    }
    if target.node_id.trim().is_empty() {
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowIdentityMissing);
    }
    // 调用方声明的行身份必须与帧里那行一致——否则"要切的那行"和"证到的那行"不是同一行。
    if target.node_id.trim() != target_row_id.trim() {
        return TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous);
    }
    if target.project_group.trim().is_empty() {
        return TargetRowProof::NotNavigable(TargetRowRefusal::GroupUnproven);
    }
    // 同标题若还出现在别的组实例里，组证据不足以为凭。
    if report.rows.iter().any(|row| {
        row.label_hash == approved.title_hash && row.project_group != target.project_group
    }) {
        return TargetRowProof::NotNavigable(TargetRowRefusal::GroupUnproven);
    }
    // **必须**恰有一个 current 行。零个或两个以上都意味着"当前会话"本身不可证，
    // 此时既不能声称目标已是当前，也不能声称目标不是当前——按帧不可信处理。
    // （复审 Critical：原实现只在目标行上读 `aria_current`，因此"两个 row 同为
    //  current"或"没有任何 current"都能通过，混合帧还会被读成 `NavigatedConfirmed`。）
    let current_rows: Vec<&SidebarRow> =
        report.rows.iter().filter(|row| row.aria_current).collect();
    if current_rows.len() != 1 {
        return TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted);
    }
    // 唯一 current 行必须自报可读且带组实例，否则"当前是谁"仍不可证。
    let current = current_rows[0];
    if !current.in_readable_region
        || current.workspace_key.trim().is_empty()
        || current.project_group.trim().is_empty()
    {
        return TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted);
    }
    // 窗口标题必须与该 current 行一致：混合帧（标题 A、current B）一律不可信。
    if report.window_title_hash.is_empty() || report.window_title_hash != current.label_hash {
        return TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted);
    }
    // 已经是当前会话 → 不需要导航（阶段一负责）。
    if target.aria_current {
        return TargetRowProof::AlreadyCurrent;
    }
    // 目标行可唯一定向，且**独立证明**当前不是它。
    TargetRowProof::Navigable {
        row_identity: target.node_id.clone(),
        project_group: target.project_group.clone(),
    }
}

/// 一次导航尝试的结果。
///
/// **刻意**与 `SendOutcome` 分开：导航不碰编辑器，任何"没切到"或"效果未知"
/// 都不能被读成"可能改了草稿"。两个枚举不共享变体名，避免在上层被 `match` 混用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationOutcome {
    /// 没有触发任何导航（目标已是当前、或证明不成立）。
    NotNavigated,
    /// 导航请求已发出，但**未确认**目标已加载。此后不得填入或提交。
    NavigatedUnconfirmed,
    /// 导航回执丢失或效果未知。此后不得重试导航，只能只读核对。
    NavigationUnknown,
    /// 已独立确认目标会话成为当前会话。**这不等于允许发送**——仍须通过阶段一的
    /// 当前会话绑定核证与写入准入。
    NavigatedConfirmed,
}

/// 导航阶段的账本阶段。与 `ActionPhase` 分开，理由同 [`NavigationOutcome`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationPhase {
    /// 尚未触碰任何导航控件。
    NotStarted,
    /// 即将触发目标行的语义操作——此后失败也不能再说"完全没动过"。
    AboutToNavigate,
    /// 语义操作已返回（成功或失败），效果待只读核对。
    NavigateAttempted,
    /// 已独立确认目标会话加载成功。
    Confirmed,
}

#[cfg(test)]
mod navigation_tests {
    use super::*;

    const WS: &str = "aaaa00001111";
    const ID_A: &str = "01234567-89ab-7def-8123-000000000001";
    const ID_B: &str = "01234567-89ab-7def-8123-000000000002";
    const H_A: &str = "8f434346648f";
    const H_B: &str = "9d74e011433f";

    fn host() -> HostIdentity {
        HostIdentity {
            pid: 2075,
            launch_generation: 1_795_773_975,
            login_session: 501,
        }
    }

    /// 批准目标 = B（阶段二要切到的那条）。
    fn approved_b() -> ApprovedTarget<'static> {
        ApprovedTarget {
            host: host(),
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_B,
            title_hash: H_B,
            project_label_hash: WS,
        }
    }

    fn row(hash: &str, current: bool, node: &str, group: &str) -> SidebarRow {
        SidebarRow {
            label_hash: hash.into(),
            aria_current: current,
            in_readable_region: true,
            workspace_key: WS.into(),
            node_id: node.into(),
            project_group: group.into(),
        }
    }

    fn counts(rows: usize) -> EnumerationCounts {
        EnumerationCounts {
            nodes_visited: 300,
            ax_calls: 900,
            rows_enumerated: rows,
            structural_unknown: 0,
            budget_hit: false,
            read_failed: false,
            title_recheck_stable: true,
        }
    }

    fn report(rows: Vec<SidebarRow>) -> ProbeReport {
        report_on(H_A, rows)
    }

    /// 指定窗口标题的帧。混合帧（标题与唯一 current 行不一致）必须由被测函数拒绝，
    /// 因此夹具要能独立控制标题。
    fn report_on(title: &str, rows: Vec<SidebarRow>) -> ProbeReport {
        let n = rows.len();
        ProbeReport {
            host: host(),
            window_count: 1,
            window_generation: 7,
            window_title_hash: title.into(),
            rows,
            counts: counts(n),
            groups_available: true,
        }
    }

    /// A 是当前、B 在侧栏且可定向 —— 阶段二的正例前提。
    fn desktop_on_a_b_available() -> ProbeReport {
        report(vec![
            row(H_A, true, "row-A", "g1"),
            row(H_B, false, "row-B", "g1"),
        ])
    }

    // ===== 正例 =====

    #[test]
    fn target_row_on_another_session_is_navigable() {
        assert_eq!(
            prove_target_row(&desktop_on_a_b_available(), &approved_b(), "row-B"),
            TargetRowProof::Navigable {
                row_identity: "row-B".into(),
                project_group: "g1".into(),
            }
        );
    }

    #[test]
    fn target_already_current_needs_no_navigation() {
        // 标题必须与唯一 current 行一致（B），否则是混合帧、按不可信处理。
        let desktop_on_b = report_on(
            H_B,
            vec![
                row(H_A, false, "row-A", "g1"),
                row(H_B, true, "row-B", "g1"),
            ],
        );
        assert_eq!(
            prove_target_row(&desktop_on_b, &approved_b(), "row-B"),
            TargetRowProof::AlreadyCurrent,
            "目标已是当前会话时不得再导航"
        );
    }

    // ===== 反例：全部返回 NotNavigable，且各自拒因不同 =====

    #[test]
    fn absent_target_row_is_not_navigable() {
        let only_a = report(vec![row(H_A, true, "row-A", "g1")]);
        assert_eq!(
            prove_target_row(&only_a, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::RowAbsent)
        );
    }

    #[test]
    fn duplicate_target_rows_are_ambiguous() {
        let two_b = report(vec![
            row(H_A, true, "row-A", "g1"),
            row(H_B, false, "row-B1", "g1"),
            row(H_B, false, "row-B2", "g1"),
        ]);
        assert_eq!(
            prove_target_row(&two_b, &approved_b(), "row-B1"),
            TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous)
        );
    }

    #[test]
    fn wrong_row_identity_is_ambiguous_not_a_guess() {
        // 调用方说要切 row-X，但帧里唯一可定向的是 row-B。
        assert_eq!(
            prove_target_row(&desktop_on_a_b_available(), &approved_b(), "row-X"),
            TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous)
        );
    }

    #[test]
    fn empty_declared_row_identity_is_refused() {
        assert_eq!(
            prove_target_row(&desktop_on_a_b_available(), &approved_b(), "  "),
            TargetRowProof::NotNavigable(TargetRowRefusal::RowIdentityMissing)
        );
    }

    #[test]
    fn unreadable_target_row_is_not_navigable() {
        let mut b = row(H_B, false, "row-B", "g1");
        b.in_readable_region = false;
        let r = report(vec![row(H_A, true, "row-A", "g1"), b]);
        // 同名行不可读 → 既不能证明是这一行、也不能证明不是 → 保守判歧义。
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous)
        );
    }

    // ===== R3 反例：导航"确认"不等于发送准入 =====

    /// 导航报 `NavigatedConfirmed` 只证明**视图换了**，不证明核证通过。
    ///
    /// 这里构造"切了但没切到"：批准 B（`approved_for` 指向 H_B/ID_B），
    /// 而重采拿到的仍是**停 A 的旧帧**。若实现拿导航回执当准入、跳过重采核证，
    /// 这条就会以 `Matched` 放行并发到 A —— 串会话。
    #[test]
    fn navigation_confirmation_does_not_admit_send_without_recheck() {
        use super::binding_tests_support::*;

        // 批准的意图是 B。
        let approved_b = approved_target_for(
            &host(),
            2,
            2,
            "01234567-89ab-7def-8123-000000000002",
            WS,
            H_B,
            WS,
            7,
        )
        .expect("approved B");

        // 重采仍是停 A 的旧帧：导航"确认"不得替代这一步的核证。
        let stale = report_for_resolver(H_A);
        let verdict =
            resolve_current_target_for_row(&stale, &approved_b, "row-current", "g1", |h| {
                resolver_idx(h)
            });
        assert_ne!(
            verdict,
            TargetBinding::Matched,
            "导航回执不得替代重采后的核证：仍停 A 的旧帧必须拒绝"
        );

        // 对照：真正切到 B 的帧才放行（证明上面拒绝的是"没切到"，不是"永远拒"）。
        let fresh = report_for_resolver(H_B);
        assert_eq!(
            resolve_current_target_for_row(&fresh, &approved_b, "row-current", "g1", |h| {
                resolver_idx(h)
            }),
            TargetBinding::Matched,
            "切到 B 后应放行；否则上面的断言没有判别力"
        );
    }

    /// 重采拿到目标会话的帧，但**行身份对不上**（同名另一行 / 视图复用了行）。
    ///
    /// 导航按 `(node_id, project_group)` 寻址，帧必须独立证明"当前行就是它"，
    /// 否则等于用导航意图给自己背书。
    #[test]
    fn navigation_to_same_title_but_different_row_is_refused() {
        use super::binding_tests_support::*;

        // 线程**确实对上了**（批准 B、帧是 B），只让行身份/组实例不符 ——
        // 这才是"行寻址不成立"的纯净构造；若线程不对，四态在行检查前就短路，
        // 断言到的是别的东西。
        let approved_b = approved_target_for(
            &host(),
            2,
            2,
            "01234567-89ab-7def-8123-000000000002",
            WS,
            H_B,
            WS,
            7,
        )
        .expect("approved B");
        let r = report_for_resolver(H_B);

        // 对照：行与组都对上 → Matched（证明下面拒的是行/组，不是整条路径恒拒）。
        assert_eq!(
            resolve_current_target_for_row(&r, &approved_b, "row-current", "g1", |h| {
                resolver_idx(h)
            }),
            TargetBinding::Matched,
            "对照基线应 Matched"
        );

        let wrong_row =
            resolve_current_target_for_row(&r, &approved_b, "row-somewhere-else", "g1", |h| {
                resolver_idx(h)
            });
        assert_eq!(
            wrong_row,
            TargetBinding::Unverifiable,
            "线程对上但行身份不符时，绑定不成立"
        );

        let wrong_group =
            resolve_current_target_for_row(&r, &approved_b, "row-current", "g2", |h| {
                resolver_idx(h)
            });
        assert_eq!(
            wrong_group,
            TargetBinding::Unverifiable,
            "组实例不符时不得按行身份放行"
        );
    }

    /// **复审 Critical 3 的绕过构造**：同名、同组实例，但**项目标签不同**的第二行。
    ///
    /// 旧实现把项目标签塞进候选 `filter`，于是这一行被静默排除，
    /// 后续组检查也就看不到它 —— 等价于从两个候选中挑了一个当唯一。
    #[test]
    fn same_title_with_different_project_label_is_not_silently_dropped() {
        let mut decoy = row(H_B, false, "row-B-decoy", "g1");
        decoy.workspace_key = "bbbb22223333".into(); // 同名、同组，但不同项目标签
        let r = report(vec![
            row(H_A, true, "row-A", "g1"),
            row(H_B, false, "row-B", "g1"),
            decoy,
        ]);
        // 同名行里有归属不同于批准的 → 候选仍唯一，但组证据必须被质疑；
        // 无论归到哪个拒因，**都不能**是 `Navigable`。
        assert!(
            matches!(
                prove_target_row(&r, &approved_b(), "row-B"),
                TargetRowProof::NotNavigable(_)
            ),
            "同名异项目的第二行不得被静默排除"
        );
    }

    /// **复审第二轮 Critical**：**异标题**行复用目标行的 `(node_id, project_group)`。
    ///
    /// 按下只用这两个字段寻址，所以只要帧里还有另一行共享它们，按下就不唯一。
    /// 旧测试只复制了**同标题**行，因此只验证了同名歧义，没验证地址唯一性。
    /// **复审第二轮指出的缺口**：`resolve_current_target` 与
    /// `resolve_current_target_for_row` **不是**四态等价。
    ///
    /// 这条测试把两者的关系钉死：行版本只会**降级**，永不放宽。
    #[test]
    fn row_aware_resolver_only_downgrades_never_relaxes() {
        use super::binding_tests_support::*;

        // 非空行 ID：四态里除 `Matched` 外全部原样透传，`Matched` 在行/组不符时降为 Unverifiable。
        for row_id in ["row-current", "row-other"] {
            for report_hash in [H_A, H_B] {
                let r = report_for_resolver(report_hash);
                let base =
                    resolve_current_target(&r, &approved_for_resolver(), |h| resolver_idx(h));
                let with_row = resolve_current_target_for_row(
                    &r,
                    &approved_for_resolver(),
                    row_id,
                    "g1",
                    |h| resolver_idx(h),
                );
                match base {
                    TargetBinding::Matched => {
                        // 行/组都对上才允许仍是 Matched；否则只能降级。
                        assert!(
                            matches!(
                                with_row,
                                TargetBinding::Matched | TargetBinding::Unverifiable
                            ),
                            "Matched 只能保持或降级，实际 {with_row:?}"
                        );
                    }
                    other => assert_eq!(
                        with_row, other,
                        "非 Matched 必须原样透传（row_id={row_id}, hash={report_hash}）"
                    ),
                }
            }
        }

        // 空行 ID / 空组：直接 Unverifiable，不会变成别的态（更严格，不会更宽松）。
        let r = report_for_resolver(H_A);
        for (rid, grp) in [("", "g1"), ("row-current", "")] {
            assert_eq!(
                resolve_current_target_for_row(&r, &approved_for_resolver(), rid, grp, |h| {
                    resolver_idx(h)
                }),
                TargetBinding::Unverifiable
            );
        }
    }

    #[test]
    fn different_title_row_reusing_the_press_address_is_not_navigable() {
        let mut hijack = row(H_A, false, "row-B", "g1"); // 标题不同，但地址与目标完全相同
        hijack.label_hash = "cccccccccccc".into();
        let r = report_on(
            H_A,
            vec![
                row(H_A, true, "row-A", "g1"),
                row(H_B, false, "row-B", "g1"),
                hijack,
            ],
        );
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous),
            "异标题行不得复用目标行的按下地址"
        );
    }

    /// 同名行复用目标行的 `(node_id, project_group)`：按下只用这两个字段寻址，
    /// 所以重复身份必须被拒。
    #[test]
    fn duplicate_row_identity_under_same_group_is_not_navigable() {
        let clone = row(H_B, false, "row-B", "g1"); // 与目标行完全相同的身份
        let r = report(vec![
            row(H_A, true, "row-A", "g1"),
            row(H_B, false, "row-B", "g1"),
            clone,
        ]);
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous)
        );
    }

    #[test]
    fn untrusted_frame_never_yields_navigable() {
        let mut r = desktop_on_a_b_available();
        r.counts.read_failed = true;
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted)
        );

        // 宿主换代同样不可信。
        let mut r2 = desktop_on_a_b_available();
        r2.host.pid = 9999;
        assert_eq!(
            prove_target_row(&r2, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted)
        );

        // 视图代际不符。
        let mut r3 = desktop_on_a_b_available();
        r3.window_generation = 8;
        assert_eq!(
            prove_target_row(&r3, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted)
        );
    }

    #[test]
    fn same_title_in_two_group_instances_is_not_navigable() {
        // 同名 B 出现在两个组实例：组证据不足，不能定向。
        let r = report(vec![
            row(H_A, true, "row-A", "g1"),
            row(H_B, false, "row-B", "g1"),
            row(H_B, false, "row-B-other-group", "g2"),
        ]);
        // 多行同标题 → 先判歧义（更强的拒绝）。
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::RowAmbiguous)
        );
    }

    // ===== Task 7：导航前的草稿保护边界（离线否证）=====
    //
    // 关键结论：`prove_target_row` **只**看侧栏行，看不到源会话编辑区的草稿。
    // 因此"源会话有草稿时不得导航"这条保护**不可能**在本内核里成立——必须由调用方
    // 另外证明（读源编辑器）。这不是疏忽，是必须在接线时显式补上的前置条件。

    #[test]
    fn row_proof_cannot_see_source_draft_so_caller_must_check_it() {
        // 一帧里 A 是当前且**有草稿**，B 可定向。行证明仍返回 `Navigable`——
        // 证明它对本机草稿完全无感。
        let r = desktop_on_a_b_available();
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::Navigable {
                row_identity: "row-B".into(),
                project_group: "g1".into(),
            },
            "行证明不含编辑器信息；草稿保护必须由调用方另行证明"
        );
        // 这一条是**如实申报的能力边界**，不是待修的缺陷：
        // 侧栏行采样里没有编辑器字段，编造一个"草稿为空"的结论才是缺陷。
        // （早期版本用"`Debug` 文本不含 draft"来"证明"这一点——那是空测，
        //  复审 Minor 已指出。真正的证据是 `ProbeReport` / `SidebarRow` 的字段定义里
        //  就没有任何草稿字段，这条边界因此不可能在本内核里成立。）
    }

    #[test]
    fn row_proof_does_not_consult_disk_activity_or_window_focus() {
        // 计划的禁止项：不靠标题、焦点、行序号、磁盘活跃度单独裁决。
        // 结构性证明：`ProbeReport` 里没有前台/焦点/活跃度字段被 `prove_target_row` 读取。
        let r = desktop_on_a_b_available();
        // 同一帧两次调用必须同结论（纯函数，无隐藏状态）。
        let first = prove_target_row(&r, &approved_b(), "row-B");
        let second = prove_target_row(&r, &approved_b(), "row-B");
        assert_eq!(first, second);
        // **正例**：把那个非 current 的无关行换一个标题，结论不变——
        // 证明定向只依赖目标行自身的 label_hash + 项目标签 + 行身份 + 组实例，
        // 不依赖无关行的标题、也不依赖任何焦点/活跃度字段。
        let mut with_other_title = desktop_on_a_b_available();
        for row in with_other_title.rows.iter_mut() {
            if row.label_hash == H_A {
                row.label_hash = "ffffffffffff".into();
            }
        }
        // 标题也要跟着变，否则混合帧被拒——这里只改无关行、同步窗口标题。
        with_other_title.window_title_hash = "ffffffffffff".into();
        assert_eq!(
            prove_target_row(&with_other_title, &approved_b(), "row-B"),
            first,
            "无关行的标题不参与目标行定向"
        );
    }

    // ===== 复审 Critical 3 的三条构造（必须全部被拒）=====

    #[test]
    fn two_current_rows_never_yield_navigable() {
        // 原实现只看目标行的 `aria_current`，因此"两个 row 同为 current"能通过。
        let r = report_on(
            H_A,
            vec![row(H_A, true, "row-A", "g1"), row(H_B, true, "row-B", "g1")],
        );
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted),
            "两个 current 行意味着当前会话不可证"
        );
    }

    #[test]
    fn no_current_row_never_yields_navigable() {
        // 清掉所有 current：既不能声称目标已是当前，也不能声称不是。
        let r = report_on(
            H_A,
            vec![
                row(H_A, false, "row-A", "g1"),
                row(H_B, false, "row-B", "g1"),
            ],
        );
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted)
        );
    }

    #[test]
    fn mixed_frame_title_mismatch_never_yields_navigable() {
        // 标题仍为 A、但 B 被标成 current —— 正是原实现读成 `NavigatedConfirmed` 的混合帧。
        let r = report_on(
            H_A,
            vec![
                row(H_A, false, "row-A", "g1"),
                row(H_B, true, "row-B", "g1"),
            ],
        );
        assert_eq!(
            prove_target_row(&r, &approved_b(), "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted),
            "标题与唯一 current 行不一致 = 混合帧"
        );
    }

    #[test]
    fn all_zero_host_identity_never_yields_navigable() {
        // 宿主三段与视图代际全零：原实现只要求"相等"，因此全零也能通过。
        let mut r = desktop_on_a_b_available();
        r.host.pid = 0;
        r.host.launch_generation = 0;
        r.host.login_session = 0;
        r.window_generation = 0;
        let mut approved = approved_b();
        approved.host.pid = 0;
        approved.host.launch_generation = 0;
        approved.host.login_session = 0;
        approved.window_generation = 0;
        assert_eq!(
            prove_target_row(&r, &approved, "row-B"),
            TargetRowProof::NotNavigable(TargetRowRefusal::FrameUntrusted),
            "全零宿主不是可核验的宿主"
        );
    }

    #[test]
    fn navigation_outcome_and_send_outcome_never_share_variant_names() {
        // 这是编译期契约的运行时影子：两个枚举的变体名不得相同，
        // 否则上层 match 时会把"导航未知"读成"编辑区可能已改"。
        assert_ne!(
            format!("{:?}", NavigationOutcome::NotNavigated),
            format!(
                "{:?}",
                crate::services::codex_desktop_ax_sender::SendOutcome::NotTouched
            )
        );
        assert_ne!(
            format!("{:?}", NavigationOutcome::NavigationUnknown),
            format!(
                "{:?}",
                crate::services::codex_desktop_ax_sender::SendOutcome::MaybeSubmitted
            )
        );
    }
}

// ============================================================
// 阶段二：导航动作核（Task 6 第二半）
// ============================================================

/// 一次导航语义操作失败。**效果未知**——按下可能已生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationPressError {
    /// AX 调用返回失败。
    Failed,
}

/// 导航动作的**假**执行器。真实实现不在本模块；这里只证明调用顺序与次数。
pub trait NavigationProvider {
    /// 对目标行做**一次**语义操作（真实实现 = 单次 `AXPress`）。
    ///
    /// 返回 `Err` 时**效果未知**——按下可能已生效。
    fn press_target_row(
        &mut self,
        row_identity: &str,
        project_group: &str,
    ) -> Result<(), NavigationPressError>;
    /// 只读重采样，用于确认目标是否已成为当前会话。
    fn resample(&self) -> Option<ProbeReport>;
}

/// 一次导航请求。
#[derive(Debug, Clone, Copy)]
pub struct NavigationRequest<'a> {
    pub action_id: &'a str,
    pub approved: &'a ApprovedTarget<'a>,
    pub target_row_id: &'a str,
}

/// 导航结果 + 已到达阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigationReport {
    pub outcome: NavigationOutcome,
    pub phase: NavigationPhase,
    pub refusal: Option<TargetRowRefusal>,
}

impl NavigationReport {
    fn refuse(phase: NavigationPhase, refusal: TargetRowRefusal) -> Self {
        // 阶段决定档位：可能已经按下去过的动作不能自称 `NotNavigated`。
        let outcome = match phase {
            NavigationPhase::NotStarted => NavigationOutcome::NotNavigated,
            NavigationPhase::AboutToNavigate => NavigationOutcome::NavigationUnknown,
            NavigationPhase::NavigateAttempted => NavigationOutcome::NavigatedUnconfirmed,
            NavigationPhase::Confirmed => NavigationOutcome::NavigatedConfirmed,
        };
        Self {
            outcome,
            phase,
            refusal: Some(refusal),
        }
    }
}

/// 导航动作账本。跨**调用**的防重靠它，而不是靠"函数里只写了一次 press"。
///
/// 复审 Important：`navigate_once` 内部不重按，但同一 `action_id` 被上层重复调用时
/// 仍会再按一次。所以"恰好一次"必须由账本保证。
pub trait NavigationJournal {
    /// 登记该动作即将导航，并**绑定它的目标**。已存在则返回 `false`（重复请求）。
    ///
    /// 复审第二轮 Important：只按 `action_id` 记阶段会让"换了目标再重放"直接借用
    /// 上一次的确认。目标必须一起存。
    fn register(&mut self, action_id: &str, target_row_id: &str, target_thread: &str) -> bool;
    /// 该动作是否**对同一目标**已确认。
    fn is_confirmed(&self, action_id: &str, target_row_id: &str, target_thread: &str) -> bool;
    fn mark_confirmed(&mut self, action_id: &str, target_row_id: &str, target_thread: &str);
}

/// 内存导航账本（测试与夹具）。
#[derive(Debug, Default)]
pub struct InMemoryNavigationJournal {
    phases: std::collections::BTreeMap<String, NavigationPhase>,
    targets: std::collections::BTreeMap<String, (String, String)>,
}

impl NavigationJournal for InMemoryNavigationJournal {
    fn register(&mut self, action_id: &str, target_row_id: &str, target_thread: &str) -> bool {
        if self.phases.contains_key(action_id) {
            return false;
        }
        self.phases
            .insert(action_id.to_string(), NavigationPhase::AboutToNavigate);
        self.targets.insert(
            action_id.to_string(),
            (target_row_id.to_string(), target_thread.to_string()),
        );
        true
    }

    fn is_confirmed(&self, action_id: &str, target_row_id: &str, target_thread: &str) -> bool {
        self.phases.get(action_id) == Some(&NavigationPhase::Confirmed)
            && self.targets.get(action_id)
                == Some(&(target_row_id.to_string(), target_thread.to_string()))
    }

    fn mark_confirmed(&mut self, action_id: &str, target_row_id: &str, target_thread: &str) {
        self.phases
            .insert(action_id.to_string(), NavigationPhase::Confirmed);
        self.targets.insert(
            action_id.to_string(),
            (target_row_id.to_string(), target_thread.to_string()),
        );
    }
}

impl InMemoryNavigationJournal {
    pub fn phase(&self, action_id: &str) -> Option<NavigationPhase> {
        self.phases.get(action_id).copied()
    }
}

/// 执行一次导航：证明目标行 → 单次操作 → 只读确认。
///
/// **不做**的事：不切回源会话、失败不重试、不换通道、不碰编辑器。
/// 成功也只到 `NavigatedConfirmed`——**不等于**允许发送。
///
/// **跨调用防重**：同一 `action_id` 第二次进入时直接走 `NavigationUnknown`
/// （已可能按过），不会再次 `press`。
pub fn navigate_once(
    provider: &mut impl NavigationProvider,
    journal: &mut impl NavigationJournal,
    request: &NavigationRequest<'_>,
    initial: &ProbeReport,
    max_polls: usize,
) -> NavigationReport {
    // 0) 跨调用防重优先于一切：已确认过的动作直接回既有结论，
    //    未确认的旧动作**不许**再来一次。
    if !journal.register(
        request.action_id,
        request.target_row_id,
        request.approved.thread_id,
    ) {
        if journal.is_confirmed(
            request.action_id,
            request.target_row_id,
            request.approved.thread_id,
        ) {
            return NavigationReport {
                outcome: NavigationOutcome::NavigatedConfirmed,
                phase: NavigationPhase::Confirmed,
                refusal: None,
            };
        }
        return NavigationReport {
            outcome: NavigationOutcome::NavigationUnknown,
            phase: NavigationPhase::NavigateAttempted,
            refusal: Some(TargetRowRefusal::FrameUntrusted),
        };
    }
    // 1) 先只读证明目标行可唯一定向。这一步不产生任何副作用。
    match prove_target_row(initial, request.approved, request.target_row_id) {
        TargetRowProof::AlreadyCurrent => NavigationReport {
            outcome: NavigationOutcome::NotNavigated,
            phase: NavigationPhase::NotStarted,
            refusal: None,
        },
        TargetRowProof::NotNavigable(refusal) => {
            NavigationReport::refuse(NavigationPhase::NotStarted, refusal)
        }
        TargetRowProof::Navigable {
            row_identity,
            project_group,
        } => {
            // 2) 唯一一次语义操作。成败都不再按第二次。
            let press = provider.press_target_row(&row_identity, &project_group);
            // 3) 只读确认：无论按成功与否都要重采样——按失败也可能已经生效。
            let mut confirmed = false;
            for _ in 0..max_polls {
                let Some(next) = provider.resample() else {
                    continue;
                };
                if prove_target_row(&next, request.approved, request.target_row_id)
                    == TargetRowProof::AlreadyCurrent
                {
                    confirmed = true;
                    break;
                }
            }
            if press.is_err() {
                // 回执失败，但只读核对可能已确认。**确认了就必须落账**——
                // 复审第二轮 Important：漏记会让重放从 Confirmed 跌成 Unknown。
                if confirmed {
                    journal.mark_confirmed(
                        request.action_id,
                        request.target_row_id,
                        request.approved.thread_id,
                    );
                }
                // 效果未知：既不能声称切成功，也不能声称没切。
                return NavigationReport {
                    outcome: if confirmed {
                        NavigationOutcome::NavigatedConfirmed
                    } else {
                        NavigationOutcome::NavigationUnknown
                    },
                    phase: if confirmed {
                        NavigationPhase::Confirmed
                    } else {
                        NavigationPhase::NavigateAttempted
                    },
                    refusal: Some(TargetRowRefusal::FrameUntrusted),
                };
            }
            if confirmed {
                journal.mark_confirmed(
                    request.action_id,
                    request.target_row_id,
                    request.approved.thread_id,
                );
                NavigationReport {
                    outcome: NavigationOutcome::NavigatedConfirmed,
                    phase: NavigationPhase::Confirmed,
                    refusal: None,
                }
            } else {
                NavigationReport {
                    outcome: NavigationOutcome::NavigatedUnconfirmed,
                    phase: NavigationPhase::NavigateAttempted,
                    refusal: Some(TargetRowRefusal::RowAbsent),
                }
            }
        }
    }
}

/// 记录型假导航提供方：只按剧本行动，并把每一次调用留在日志里。
#[derive(Debug, Default)]
pub struct FakeNavigationProvider {
    log: std::cell::RefCell<Vec<String>>,
    /// 重采样依次返回这些帧；用尽后返回 `None`。
    frames: std::cell::RefCell<std::collections::VecDeque<ProbeReport>>,
    /// 让唯一一次按键返回失败。
    fail_press: bool,
    /// 按键成功后把"桌面已切到目标"的帧推入队列（模拟 App 真实行为）。
    switch_on_press: bool,
}

impl FakeNavigationProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_frames(self, frames: Vec<ProbeReport>) -> Self {
        self.frames.borrow_mut().extend(frames);
        self
    }

    pub fn fail_press(self) -> Self {
        Self {
            fail_press: true,
            ..self
        }
    }

    pub fn switch_on_press(self) -> Self {
        Self {
            switch_on_press: true,
            ..self
        }
    }

    pub fn press_count(&self) -> usize {
        self.log
            .borrow()
            .iter()
            .filter(|entry| entry.starts_with("press:"))
            .count()
    }

    pub fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

impl NavigationProvider for FakeNavigationProvider {
    fn press_target_row(
        &mut self,
        row_identity: &str,
        project_group: &str,
    ) -> Result<(), NavigationPressError> {
        self.log
            .borrow_mut()
            .push(format!("press:{row_identity}/{project_group}"));
        if self.fail_press {
            return Err(NavigationPressError::Failed);
        }
        if self.switch_on_press {
            self.frames.borrow_mut().clear();
        }
        Ok(())
    }

    fn resample(&self) -> Option<ProbeReport> {
        self.log.borrow_mut().push("resample".into());
        self.frames.borrow_mut().pop_front()
    }
}

#[cfg(test)]
mod navigation_flow_tests {
    use super::navigation_tests_support::*;
    use super::*;

    fn req<'a>(approved: &'a ApprovedTarget<'a>) -> NavigationRequest<'a> {
        NavigationRequest {
            action_id: "nav-1",
            approved,
            target_row_id: "row-B",
        }
    }

    // ===== 零导航：这些路径的 press 次数必须是 0 =====

    #[test]
    fn target_already_current_never_presses() {
        let mut provider = FakeNavigationProvider::new().with_frames(vec![report_on_b()]);
        let mut journal = InMemoryNavigationJournal::default();
        let r = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_b(),
            3,
        );
        assert_eq!(r.outcome, NavigationOutcome::NotNavigated);
        assert_eq!(r.phase, NavigationPhase::NotStarted);
        assert_eq!(provider.press_count(), 0, "目标已是当前会话，不得导航");
    }

    #[test]
    fn unprovable_target_row_never_presses() {
        // 侧栏里没有 B 行。
        let only_a = {
            let rows = vec![row(H_A, true, "row-A", "g1")];
            let n = rows.len();
            ProbeReport {
                host: host(),
                window_count: 1,
                window_generation: 7,
                window_title_hash: H_A.into(),
                rows,
                counts: counts(n),
                groups_available: true,
            }
        };
        let mut provider = FakeNavigationProvider::new();
        let r = {
            let mut j = InMemoryNavigationJournal::default();
            navigate_once(&mut provider, &mut j, &req(&approved_b()), &only_a, 3)
        };
        assert_eq!(r.outcome, NavigationOutcome::NotNavigated);
        assert_eq!(r.refusal, Some(TargetRowRefusal::RowAbsent));
        assert_eq!(provider.press_count(), 0);
    }

    #[test]
    fn untrusted_frame_never_presses() {
        let mut bad = report_on_a();
        bad.counts.budget_hit = true;
        let mut provider = FakeNavigationProvider::new();
        let r = {
            let mut j = InMemoryNavigationJournal::default();
            navigate_once(&mut provider, &mut j, &req(&approved_b()), &bad, 3)
        };
        assert_eq!(r.refusal, Some(TargetRowRefusal::FrameUntrusted));
        assert_eq!(provider.press_count(), 0);
    }

    // ===== 正例：A→B 恰好一次 =====

    #[test]
    fn approved_navigation_presses_exactly_once_and_confirms() {
        let mut provider = FakeNavigationProvider::new().with_frames(vec![report_on_b()]);
        let mut journal = InMemoryNavigationJournal::default();
        let r = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            3,
        );
        assert_eq!(r.outcome, NavigationOutcome::NavigatedConfirmed);
        assert_eq!(r.phase, NavigationPhase::Confirmed);
        assert_eq!(r.refusal, None);
        assert_eq!(provider.press_count(), 1, "获准的 A→B 只切一次");
    }

    // ===== 反例：效果未知 / 未确认，都不再按第二次 =====

    #[test]
    fn press_failure_without_confirmation_is_unknown_and_never_retried() {
        let mut provider = FakeNavigationProvider::new()
            .fail_press()
            .with_frames(vec![report_on_a(), report_on_a()]);
        let mut journal = InMemoryNavigationJournal::default();
        let r = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(r.outcome, NavigationOutcome::NavigationUnknown);
        assert_eq!(r.phase, NavigationPhase::NavigateAttempted);
        assert_eq!(provider.press_count(), 1, "失败不得重试导航");
    }

    #[test]
    fn press_failure_that_actually_switched_is_reported_as_confirmed() {
        // 按下返回 Err，但桌面确实切过去了——只读核对优先于回执。
        let mut provider = FakeNavigationProvider::new()
            .fail_press()
            .with_frames(vec![report_on_b()]);
        let mut journal = InMemoryNavigationJournal::default();
        let r = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(r.outcome, NavigationOutcome::NavigatedConfirmed);
        assert_eq!(provider.press_count(), 1);
    }

    #[test]
    fn successful_press_without_confirmation_stays_unconfirmed() {
        let mut provider =
            FakeNavigationProvider::new().with_frames(vec![report_on_a(), report_on_a()]);
        let mut journal = InMemoryNavigationJournal::default();
        let r = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(r.outcome, NavigationOutcome::NavigatedUnconfirmed);
        assert_eq!(r.phase, NavigationPhase::NavigateAttempted);
        assert_eq!(
            provider.press_count(),
            1,
            "AX 返回成功或按钮高亮都不算加载成功，但也不得再按"
        );
    }

    /// **复审 Important 4**：同一 `action_id` 被重复调用时不得再按第二次。
    ///
    /// 函数内部"只写一次 press"不够——上层重放同一动作时仍会再按。
    #[test]
    fn same_action_id_never_presses_twice_across_calls() {
        let mut provider =
            FakeNavigationProvider::new().with_frames(vec![report_on_a(), report_on_a()]);
        let mut journal = InMemoryNavigationJournal::default();

        // 第一次：按了一次，但未确认 → NavigationUnknown/NavigatedUnconfirmed。
        let first = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            1,
        );
        assert_eq!(first.outcome, NavigationOutcome::NavigatedUnconfirmed);
        assert_eq!(provider.press_count(), 1);

        // 第二次同一 action_id：无论帧长什么样都不许再按。
        let second = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            1,
        );
        assert_eq!(provider.press_count(), 1, "同一动作跨调用不得重按");
        assert_ne!(second.outcome, NavigationOutcome::NavigatedConfirmed);
    }

    /// **复审第二轮 Important**：按下返回 `Err`、但只读核对确认成功时，确认必须落账。
    ///
    /// 旧实现漏记这一支，重放同一请求会从 `NavigatedConfirmed` 跌成 `NavigationUnknown`。
    #[test]
    fn confirmed_after_press_error_is_recorded_and_replays_as_confirmed() {
        let mut provider = FakeNavigationProvider::new()
            .fail_press()
            .with_frames(vec![report_on_b()]);
        let mut journal = InMemoryNavigationJournal::default();
        let first = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(first.outcome, NavigationOutcome::NavigatedConfirmed);
        assert_eq!(journal.phase("nav-1"), Some(NavigationPhase::Confirmed));

        // 重放：必须是 Confirmed，不能跌成 Unknown，也不能再按。
        let replay = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(
            replay.outcome,
            NavigationOutcome::NavigatedConfirmed,
            "已确认的动作重放不得跌成 Unknown"
        );
        assert_eq!(provider.press_count(), 1);
    }

    /// **复审第二轮 Important**：账本绑定目标，换一个行再重放不能借用旧确认。
    #[test]
    fn confirmation_is_not_lent_to_a_different_target_row() {
        let mut provider = FakeNavigationProvider::new().with_frames(vec![report_on_b()]);
        let mut journal = InMemoryNavigationJournal::default();
        let first = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(first.outcome, NavigationOutcome::NavigatedConfirmed);

        // 同一 action_id，但目标行换成别的：不得直接拿到 Confirmed。
        let other_target = NavigationRequest {
            action_id: "nav-1",
            approved: &approved_b(),
            target_row_id: "row-OTHER",
        };
        let replay = navigate_once(
            &mut provider,
            &mut journal,
            &other_target,
            &report_on_a(),
            2,
        );
        assert_ne!(
            replay.outcome,
            NavigationOutcome::NavigatedConfirmed,
            "换了目标不得借用旧确认"
        );
        assert_eq!(provider.press_count(), 1, "重放不得再按");
    }

    /// 已确认的动作被重放：如实回既有结论，仍不再按。
    #[test]
    fn confirmed_action_replay_returns_same_verdict_without_pressing() {
        let mut provider = FakeNavigationProvider::new().with_frames(vec![report_on_b()]);
        let mut journal = InMemoryNavigationJournal::default();
        let first = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(first.outcome, NavigationOutcome::NavigatedConfirmed);
        assert_eq!(provider.press_count(), 1);

        let replay = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(replay.outcome, NavigationOutcome::NavigatedConfirmed);
        assert_eq!(journal.phase("nav-1"), Some(NavigationPhase::Confirmed));
        assert_eq!(provider.press_count(), 1, "重放不得再按");
    }

    #[test]
    fn no_frame_available_after_press_is_unconfirmed() {
        let mut provider = FakeNavigationProvider::new();
        let mut journal = InMemoryNavigationJournal::default();
        let r = navigate_once(
            &mut provider,
            &mut journal,
            &req(&approved_b()),
            &report_on_a(),
            2,
        );
        assert_eq!(r.outcome, NavigationOutcome::NavigatedUnconfirmed);
        assert_eq!(provider.press_count(), 1);
    }
}

/// 与 `navigation_tests` 共用的夹具。
#[cfg(test)]
mod navigation_tests_support {
    use super::*;

    pub const WS: &str = "aaaa00001111";
    pub const ID_B: &str = "01234567-89ab-7def-8123-000000000002";
    pub const H_A: &str = "8f434346648f";
    pub const H_B: &str = "9d74e011433f";

    pub fn host() -> HostIdentity {
        HostIdentity {
            pid: 2075,
            launch_generation: 1_795_773_975,
            login_session: 501,
        }
    }

    pub fn approved_b() -> ApprovedTarget<'static> {
        ApprovedTarget {
            host: host(),
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_B,
            title_hash: H_B,
            project_label_hash: WS,
        }
    }

    pub fn row(hash: &str, current: bool, node: &str, group: &str) -> SidebarRow {
        SidebarRow {
            label_hash: hash.into(),
            aria_current: current,
            in_readable_region: true,
            workspace_key: WS.into(),
            node_id: node.into(),
            project_group: group.into(),
        }
    }

    pub fn counts(rows: usize) -> EnumerationCounts {
        EnumerationCounts {
            nodes_visited: 300,
            ax_calls: 900,
            rows_enumerated: rows,
            structural_unknown: 0,
            budget_hit: false,
            read_failed: false,
            title_recheck_stable: true,
        }
    }

    pub fn report_on_a() -> ProbeReport {
        let rows = vec![
            row(H_A, true, "row-A", "g1"),
            row(H_B, false, "row-B", "g1"),
        ];
        let n = rows.len();
        ProbeReport {
            host: host(),
            window_count: 1,
            window_generation: 7,
            window_title_hash: H_A.into(),
            rows,
            counts: counts(n),
            groups_available: true,
        }
    }

    pub fn report_on_b() -> ProbeReport {
        let rows = vec![
            row(H_A, false, "row-A", "g1"),
            row(H_B, true, "row-B", "g1"),
        ];
        let n = rows.len();
        ProbeReport {
            host: host(),
            window_count: 1,
            window_generation: 7,
            window_title_hash: H_B.into(),
            rows,
            counts: counts(n),
            groups_available: true,
        }
    }
}

/// 逐态对照测试用的夹具（与 `binding_tests` 同形，但独立以免互相干扰）。
#[cfg(test)]
mod binding_tests_support {
    use super::*;

    pub const WS: &str = "aaaa00001111";
    pub const ID_A: &str = "01234567-89ab-7def-8123-000000000001";
    pub const H_A: &str = "8f434346648f";
    pub const H_B: &str = "9d74e011433f";

    pub fn host() -> HostIdentity {
        HostIdentity {
            pid: 2075,
            launch_generation: 1_795_773_975,
            login_session: 501,
        }
    }

    pub fn approved_for_resolver() -> ApprovedTarget<'static> {
        ApprovedTarget {
            host: host(),
            window_generation: 7,
            workspace_key: WS,
            thread_id: ID_A,
            title_hash: H_A,
            project_label_hash: WS,
        }
    }

    pub fn resolver_idx(hash: &str) -> IndexLookup {
        if hash == H_A {
            IndexLookup::Unique {
                id: ID_A.into(),
                cwd: WS.into(),
            }
        } else {
            IndexLookup::Unique {
                id: "01234567-89ab-7def-8123-000000000002".into(),
                cwd: WS.into(),
            }
        }
    }

    pub fn report_for_resolver(current_hash: &str) -> ProbeReport {
        let other = if current_hash == H_A { H_B } else { H_A };
        let rows = vec![
            SidebarRow {
                label_hash: current_hash.into(),
                aria_current: true,
                in_readable_region: true,
                workspace_key: WS.into(),
                node_id: "row-current".into(),
                project_group: "g1".into(),
            },
            SidebarRow {
                label_hash: other.into(),
                aria_current: false,
                in_readable_region: true,
                workspace_key: WS.into(),
                node_id: "row-other".into(),
                project_group: "g1".into(),
            },
        ];
        let n = rows.len();
        ProbeReport {
            host: host(),
            window_count: 1,
            window_generation: 7,
            window_title_hash: current_hash.into(),
            rows,
            counts: EnumerationCounts {
                nodes_visited: 300,
                ax_calls: 900,
                rows_enumerated: n,
                structural_unknown: 0,
                budget_hit: false,
                read_failed: false,
                title_recheck_stable: true,
            },
            groups_available: true,
        }
    }
}

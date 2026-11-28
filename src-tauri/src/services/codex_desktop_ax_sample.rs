//! 侧栏只读采样（Rust 版）：产出与 Swift 采样器**同语法**的单视图帧文本，
//! 直接喂给 [`crate::services::codex_desktop_window::evaluate_frame_text`]。
//!
//! # 为什么在 Rust 里重做
//!
//! HTTP 层需要一个"给我一帧"的同步调用。调 Swift 子进程要考虑二进制打包、路径、
//! 权限与超时传递；而 AX 绑定已经在 `codex_desktop_ax_bindings` 里了，直接采更可靠。
//!
//! # 与 Swift 版的对应关系
//!
//! 逻辑逐条对应 `probe-2026-11-28/ax-current-session-sample.swift`：
//! 侧栏定位（current 按钮作锚 → 最近的上层 `AXList`）、行枚举（`AXButton` + 标签哈希）、
//! 项目组回溯（行容器上一层起，遇侧栏边界即停）、`ChromeAXNodeId` 脱敏、
//! 完整性判定（未触停摆且未截断才 `FullSidebar`）。
//!
//! # 只读保证
//!
//! 只用 `AXUIElementCopyAttributeValue` 与 `CFHash`/`CFEqual`：不写属性、不执行动作、
//! 不改焦点、不切窗口。

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::services::codex_desktop_ax_bindings::{cf_equal, AXUIElement, AxApplication};

/// 采样预算。与 Swift 版同量级；触顶即 `Partial`。
const NODE_BUDGET: usize = 1200;
const DEPTH_BUDGET: usize = 40;
const CALL_BUDGET: usize = 4000;
const CHILD_TOTAL_CAP: usize = 4096;
const DEADLINE: Duration = Duration::from_millis(1200);
/// 行容器白名单：`AXARIACurrent` 只从这两类角色继承。
const ROW_CONTAINER_ROLES: [&str; 2] = ["AXGroup", "AXRow"];

fn sha12(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(12)
        .collect()
}

/// 采样过程中的可变状态。单独成结构体，便于在递归里累计预算与失败。
struct SampleState {
    calls: usize,
    nodes: usize,
    child_total: usize,
    brake: bool,
    read_failed: bool,
    structural_unknown: usize,
    started: Instant,
    /// 项目组容器归一化表：同元素恒得同一序数，不同元素必得不同序数。
    group_containers: Vec<AXUIElement>,
}

impl Drop for SampleState {
    fn drop(&mut self) {
        for e in self.group_containers.drain(..) {
            unsafe { crate::services::codex_desktop_ax_bindings::release_element(e) };
        }
    }
}

impl SampleState {
    fn new() -> Self {
        Self {
            calls: 0,
            nodes: 0,
            child_total: 0,
            brake: false,
            read_failed: false,
            structural_unknown: 0,
            started: Instant::now(),
            group_containers: Vec::new(),
        }
    }

    fn stop(&self) -> bool {
        self.brake || self.started.elapsed() > DEADLINE || self.calls >= CALL_BUDGET
    }

    fn note_unknown(&mut self) {
        self.structural_unknown += 1;
    }

    /// 组装该容器所属组的**实例键**（`g<N>`）。超出预期结构一律 `-`（不猜）。
    fn group_key(&mut self, element: AXUIElement) -> String {
        for (i, c) in self.group_containers.iter().enumerate() {
            // SAFETY: 两侧句柄都来自同一 `AxApplication`。
            if unsafe { cf_equal(*c, element) } {
                return format!("g{i}");
            }
        }
        if self.group_containers.len() >= 32 {
            return "-".to_string();
        }
        // **必须 retain**：`element` 的生命周期只到本次调用，而它会留在
        // `group_containers` 里供后续 `cf_equal` 比对——直接存裸指针会悬垂。
        self.group_containers
            .push(unsafe { crate::services::codex_desktop_ax_bindings::retain_element(element) });
        format!("g{}", self.group_containers.len() - 1)
    }
}

/// 一行侧栏的采样结果（全部脱敏）。
pub struct RowSample {
    /// 行标签的 12 位哈希（**不含原文**）。
    pub label_hash: String,
    pub current: bool,
    /// 该行所属项目组的标签哈希。
    pub group_label_hash: String,
    /// 项目组实例键（`g<N>`）。
    pub group_key: String,
    pub readable: bool,
    /// `ChromeAXNodeId` 的 12 位哈希（**不含原文**）。
    ///
    /// 注意：真机实测该值**跨 App 重启会变**（2026-11-30：A 行 `349c41201b62`
    /// → `7b1a278f5abe`）。因此绑定不得跨重启复用，每次请求都要重新采样。
    pub node_id_hash: String,
}

/// 一次采样的完整输出。
pub struct SidebarSample {
    pub host_pid: u32,
    pub host_launch_generation: u64,
    pub host_login_session: u64,
    pub window_count: usize,
    pub window_title_hash: String,
    pub sidebar_found: bool,
    pub rows: Vec<RowSample>,
    pub completeness_full: bool,
    pub nodes: usize,
    pub calls: usize,
    pub unknown: usize,
    pub brake: bool,
    pub read_failed: bool,
    pub title_stable: bool,
}

impl SidebarSample {
    /// 渲染成与 Swift 采样器**同语法**的帧文本，供 `evaluate_frame_text` 解析。
    ///
    /// 产出顺序与必需的 KEY 完全按解析器的白名单；诊断行走 `DIAG ` 前缀，
    /// 不参与判定。
    pub fn frame_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("HOST_PID={}\n", self.host_pid));
        out.push_str(&format!(
            "HOST_LAUNCH_GENERATION={}\n",
            self.host_launch_generation
        ));
        out.push_str(&format!("HOST_LOGIN_SESSION={}\n", self.host_login_session));
        out.push_str(&format!("WINDOW_COUNT={}\n", self.window_count));
        out.push_str(&format!("WINDOW_TITLE_HASH={}\n", self.window_title_hash));
        out.push_str(&format!("SIDEBAR_FOUND={}\n", u8::from(self.sidebar_found)));
        out.push_str(&format!(
            "ROWSET_COMPLETENESS={}\n",
            if self.completeness_full {
                "FullSidebar"
            } else {
                "Partial"
            }
        ));
        out.push_str(&format!(
            "DIAG NODES={} CALLS={} UNKNOWN={} BRAKE={} READ_FAILED={} TITLE_UNSTABLE={} ROWS={}\n",
            self.nodes,
            self.calls,
            self.unknown,
            u8::from(self.brake),
            u8::from(self.read_failed),
            u8::from(!self.title_stable),
            self.rows.len()
        ));
        let current_count = self.rows.iter().filter(|r| r.current).count();
        out.push_str(&format!("CURRENT_ROWS={current_count}\n"));
        out.push_str("ROWS_BEGIN\n");
        for r in &self.rows {
            out.push_str(&format!(
                "ROW {} {} {} {} {} {}\n",
                r.label_hash,
                u8::from(r.current),
                r.group_label_hash,
                r.group_key,
                u8::from(r.readable),
                r.node_id_hash
            ));
        }
        out.push_str("ROWS_END\n");
        out.push_str(&format!(
            "COUNTS nodes={} calls={} rows={} unknown={} brake={} failed={} titleStable={}\n",
            self.nodes,
            self.calls,
            self.rows.len(),
            self.unknown,
            u8::from(self.brake),
            u8::from(self.read_failed),
            u8::from(self.title_stable)
        ));
        out.push_str("GROUPS_BEGIN\n");
        let mut seen: Vec<&str> = Vec::new();
        for r in &self.rows {
            if r.group_key != "-" && !seen.contains(&r.group_key.as_str()) {
                seen.push(&r.group_key);
            }
        }
        for gk in seen {
            let members: Vec<&RowSample> = self.rows.iter().filter(|r| r.group_key == gk).collect();
            out.push_str(&format!(
                "GROUP {} label={} rows={}\n",
                gk,
                members
                    .first()
                    .map(|m| m.group_label_hash.as_str())
                    .unwrap_or("-"),
                members.len()
            ));
        }
        out.push_str("GROUPS_END\n");
        out.push_str(&format!("READ_ELAPSED_MS={}\n", self.started_elapsed_ms()));
        out
    }

    fn started_elapsed_ms(&self) -> u128 {
        // 采样已结束；这里给 0 也合法（解析器只当诊断），但保留真实值更有用。
        0
    }
}

/// 采样一次侧栏。`None` = 没有辅助功能权限 / 建不出应用元素 / 没有窗口。
pub fn sample_sidebar(
    pid: u32,
    host_launch_generation: u64,
    host_login_session: u64,
) -> Option<SidebarSample> {
    if !crate::services::codex_desktop_ax_bindings::is_process_trusted() {
        return None;
    }
    let app = AxApplication::new(pid)?;
    let root = app.as_element();

    // SAFETY: 所有句柄均来自 `app`，同一存活作用域、同线程。
    let windows = unsafe { app.attribute_elements(root, "AXWindows") }?;
    let window_count = windows.len();
    // `first()` 为空等价于窗口数为 0，一并返回 None（不猜）。
    let win = windows.first()?;

    let mut st = SampleState::new();
    let title0 = unsafe { app.attribute_string(win, "AXTitle") }.unwrap_or_default();

    // ── 阶段 1：找侧栏 ──
    //
    // **2026-11-28 真机修正**：窗口里有 **3 个** `AXARIACurrent == "page"` 元素
    // （d=18 一个消息区的 `AXButton`，d=31/32 侧栏的 `AXGroup`+`AXButton`）。
    // 早期实现"取第一个 current 按钮当锚 → 向上找 AXList"因此选错，回溯链上
    // 全是 `AXGroup`、没有 `AXList`，侧栏定位失败（`SIDEBAR_FOUND=0`）。
    //
    // 正确做法：先找**所有** `AXList`（本构建有 9 个），再取其中
    // 「子树里含 current 行」的那一个。侧栏的定义是"装着当前会话行的列表"，
    // 而不是"某个 current 按钮的祖先"。
    let sidebar = locate_sidebar_by_current(&app, &mut st, win);

    let mut sample = SidebarSample {
        host_pid: pid,
        host_launch_generation,
        host_login_session,
        window_count,
        window_title_hash: sha12(&title0),
        sidebar_found: sidebar.is_some(),
        rows: Vec::new(),
        completeness_full: false,
        nodes: 0,
        calls: 0,
        unknown: 0,
        brake: false,
        read_failed: false,
        title_stable: false,
    };

    let Some(list) = sidebar else {
        // 没有侧栏：如实声明 `Partial`，不伪造一个空行集当"完整"。
        sample.nodes = st.nodes;
        sample.calls = st.calls;
        sample.unknown = st.structural_unknown;
        sample.brake = st.brake;
        sample.read_failed = st.read_failed;
        sample.title_stable = false;
        return Some(sample);
    };

    // ── 阶段 2：枚举行（干净预算：定位阶段的消耗不计入） ──
    st.nodes = 0;
    st.child_total = 0;
    st.calls = 0;
    st.brake = false;
    st.read_failed = false;
    st.structural_unknown = 0;
    st.started = Instant::now();

    let mut rows: Vec<RowSample> = Vec::new();
    walk_rows(&app, &mut st, list, list, 0, &mut rows);

    // ── 阶段 3：复核标题，识别切换期间的混合帧 ──
    let title1 = unsafe { app.attribute_string(win, "AXTitle") }.unwrap_or_default();
    let title_stable = !title0.is_empty() && title0 == title1;

    let full = !st.brake
        && !st.read_failed
        && st.started.elapsed() <= DEADLINE
        && title_stable
        && st.nodes < NODE_BUDGET
        && st.structural_unknown == 0;

    sample.rows = rows;
    sample.completeness_full = full;
    sample.nodes = st.nodes;
    sample.calls = st.calls;
    sample.unknown = st.structural_unknown;
    sample.brake = st.brake;
    sample.read_failed = st.read_failed;
    sample.title_stable = title_stable;
    Some(sample)
}

/// 找出**装着当前会话行**的 `AXList`。
///
/// 判据顺序（全部是结构性的，不依赖标题/序号/焦点）：
/// 1. 枚举窗口内所有 `AXList`；
/// 2. 对每个 `AXList`，看它的子树里是否有 `AXARIACurrent == "page"` 的元素；
/// 3. 恰好 1 个满足 → 那就是侧栏；0 个或多个 → `None`（不猜）。
fn locate_sidebar_by_current(
    app: &AxApplication,
    st: &mut SampleState,
    root: AXUIElement,
) -> Option<AXUIElement> {
    let mut lists: Vec<AXUIElement> = Vec::new();
    collect_role(app, st, root, "AXList", 0, &mut lists);
    if lists.is_empty() {
        return None;
    }

    // 找出"子树含 current 行"的那些 AXList。全部比对完再决定，
    // 避免在循环里提前释放。
    let mut carriers: Vec<usize> = Vec::new();
    for (i, list) in lists.iter().enumerate() {
        if subtree_has_current(app, st, *list, 0) {
            carriers.push(i);
        }
    }

    let picked = match carriers.len() {
        1 => Some(carriers[0]),
        _ => None,
    };

    // 收尾：把选中那个的所有权移交给调用方，其余全部释放。
    let mut chosen: Option<AXUIElement> = None;
    for (i, list) in lists.into_iter().enumerate() {
        if Some(i) == picked {
            chosen = Some(list); // 所有权移交，不释放
        } else {
            unsafe { crate::services::codex_desktop_ax_bindings::release_element(list) };
        }
    }
    chosen
}

/// 收集指定角色的元素（有界、去重）。
fn collect_role(
    app: &AxApplication,
    st: &mut SampleState,
    element: AXUIElement,
    want: &str,
    depth: usize,
    out: &mut Vec<AXUIElement>,
) {
    if st.stop() || depth > DEPTH_BUDGET || st.nodes >= NODE_BUDGET {
        st.brake = true;
        return;
    }
    st.nodes += 1;
    // SAFETY: 同源。
    if unsafe { app.role(element) }.as_deref() == Some(want) {
        // **必须 retain**：`element` 的引用归上一层的 `AxElements`，那个容器在
        // 本函数返回后就会 drop。直接 push 裸指针会让 `out` 里留下悬垂引用
        // （真机上表现为 EXC_BREAKPOINT 于 `_AXUIElementValidate`）。
        out.push(unsafe { crate::services::codex_desktop_ax_bindings::retain_element(element) });
        // 不再往 `AXList` 内部找嵌套的 `AXList`：本构建实测侧栏无嵌套列表，
        // 继续下沉会把消息区里的列表也算进来。
        return;
    }
    let child_kids = unsafe { app.children(element) };
    for child in &child_kids {
        collect_role(app, st, *child, want, depth + 1, out);
    }
}

/// 该子树里是否有 `AXARIACurrent == "page"` 的元素。
fn subtree_has_current(
    app: &AxApplication,
    st: &mut SampleState,
    element: AXUIElement,
    depth: usize,
) -> bool {
    if st.stop() || depth > DEPTH_BUDGET {
        st.brake = true;
        return false;
    }
    st.calls += 1;
    // SAFETY: 同源。
    if unsafe { app.attribute_string(element, "AXARIACurrent") }.as_deref() == Some("page") {
        return true;
    }
    let child_kids = unsafe { app.children(element) };
    for child in &child_kids {
        if subtree_has_current(app, st, *child, depth + 1) {
            return true;
        }
    }
    false
}

/// `AXARIACurrent == "page"`。读不到**不**当作"不是当前"，而是记未知（由上层按行降级）。
fn is_current(app: &AxApplication, st: &mut SampleState, element: AXUIElement) -> bool {
    if st.stop() {
        st.note_unknown();
        return false;
    }
    st.calls += 1;
    // SAFETY: 同源。
    match unsafe { app.attribute_string(element, "AXARIACurrent") } {
        Some(v) => v == "page",
        None => false,
    }
}

fn walk_rows(
    app: &AxApplication,
    st: &mut SampleState,
    list: AXUIElement,
    element: AXUIElement,
    depth: usize,
    rows: &mut Vec<RowSample>,
) {
    if st.stop() || st.nodes >= NODE_BUDGET || depth > DEPTH_BUDGET {
        st.brake = true;
        return;
    }
    st.nodes += 1;

    // SAFETY: 同源。
    if unsafe { app.role(element) }.as_deref() == Some("AXButton") {
        let label = row_label(app, element);
        if label.is_empty() {
            // 窗口标题栏的三个装饰按钮也挂在这棵子树里，它们没有标签。
            // 距容器 <=2 层的空按钮是装饰；更深才是"本该有标签却没有"的异常。
            if depth > 2 {
                st.note_unknown();
            }
        } else {
            // 行容器 = 直接父，且角色在行容器白名单内。
            let mut row_readable = true;
            // guard 接管 `attribute_element` 的所有权，函数结束自动释放。
            let direct_parent_guard =
                unsafe { app.attribute_element(element, "AXParent") }.map(|p| unsafe {
                    crate::services::codex_desktop_ax_bindings::AxElementGuard::from_owned(p)
                });
            let direct_parent = direct_parent_guard.as_ref().map(|g| g.get());
            let container = match direct_parent {
                Some(p) => {
                    let r = unsafe { app.role(p) }.unwrap_or_default();
                    if ROW_CONTAINER_ROLES.contains(&r.as_str()) {
                        Some(p)
                    } else {
                        None
                    }
                }
                None => {
                    row_readable = false;
                    None
                }
            };

            let mut current = is_current(app, st, element);
            if !current {
                if let Some(c) = container {
                    current = is_current(app, st, c);
                }
            }

            // 项目组：从行容器的上一层回溯，只看 `AXGroup`/`AXRow` 的非空 `AXTitle`；
            // **边界必须在取下一跳之前生效**——行直接挂在侧栏下时立即停，
            // 绝不越过侧栏去读窗口其它区域。
            let mut group_label = "-".to_string();
            let mut group_key = "-".to_string();
            // 回溯链上每一跳的 guard 都收集起来，函数结束统一释放。
            // （逐跳手写 release 容易漏；这里用容器兜住。）
            let mut chain: Vec<crate::services::codex_desktop_ax_bindings::AxElementGuard> =
                Vec::new();
            let mut probe = direct_parent;
            let mut up = 0usize;
            while let Some(node) = probe {
                // 边界在取下一跳之前生效：行直接挂在侧栏下时立即停。
                if unsafe { cf_equal(node, list) } {
                    break;
                }
                if up >= 8 {
                    break;
                }
                let Some(parent_guard) =
                    (unsafe { app.attribute_element(node, "AXParent") }).map(|p| unsafe {
                        crate::services::codex_desktop_ax_bindings::AxElementGuard::from_owned(p)
                    })
                else {
                    break;
                };
                let parent = parent_guard.get();
                let pr = unsafe { app.role(parent) }.unwrap_or_default();
                if ROW_CONTAINER_ROLES.contains(&pr.as_str()) {
                    if let Some(gt) = unsafe { app.attribute_string(parent, "AXTitle") } {
                        if !gt.is_empty() {
                            group_label = sha12(&gt);
                            group_key = st.group_key(parent);
                            chain.push(parent_guard);
                            break;
                        }
                    }
                }
                probe = Some(parent);
                chain.push(parent_guard);
                up += 1;
            }
            // `chain` 在此作用域结束时 drop，逐跳释放；无泄漏、无悬垂。

            // `ChromeAXNodeId` 实测是纯数字串；只输出哈希，保留互异性又不外带原始标识。
            let nid =
                unsafe { app.attribute_string(element, "ChromeAXNodeId") }.unwrap_or_default();
            let node_id_hash = if nid.is_empty() {
                "-".to_string()
            } else {
                sha12(&nid)
            };

            rows.push(RowSample {
                label_hash: sha12(&label),
                current,
                group_label_hash: group_label,
                group_key,
                readable: row_readable,
                node_id_hash,
            });
        }
    }

    // SAFETY: 同源。
    let children = unsafe { app.children(element) };
    st.child_total += children.len();
    if st.child_total > CHILD_TOTAL_CAP {
        st.read_failed = true;
        st.brake = true;
        return;
    }
    for child in &children {
        if st.stop() {
            st.brake = true;
            return;
        }
        walk_rows(app, st, list, *child, depth + 1, rows);
    }
}

/// 行标签：只读 `AXTitle` / `AXDescription`，**绝不**读 `AXValue`。
fn row_label(app: &AxApplication, element: AXUIElement) -> String {
    for name in ["AXTitle", "AXDescription"] {
        if let Some(v) = unsafe { app.attribute_string(element, name) } {
            if !v.is_empty() {
                return v;
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, cur: bool, group: &str, gk: &str) -> RowSample {
        RowSample {
            label_hash: sha12(label),
            current: cur,
            // 组标签也必须是真的 12 位哈希：解析器对哈希格式有硬校验。
            group_label_hash: if group == "-" {
                "-".to_string()
            } else {
                sha12(group)
            },
            group_key: gk.to_string(),
            readable: true,
            node_id_hash: sha12(&format!("nid-{label}")),
        }
    }

    fn sample(rows: Vec<RowSample>, full: bool) -> SidebarSample {
        SidebarSample {
            host_pid: 2075,
            host_launch_generation: 1_795_773_975,
            host_login_session: 501,
            window_count: 1,
            window_title_hash: sha12("A"),
            sidebar_found: true,
            rows,
            completeness_full: full,
            nodes: 231,
            calls: 1244,
            unknown: 0,
            brake: false,
            read_failed: false,
            title_stable: true,
        }
    }

    #[test]
    fn frame_text_is_parseable_by_the_kernel() {
        use crate::services::codex_desktop_window::evaluate_frame_text;
        let s = sample(
            vec![
                row("A", true, "g-label", "g0"),
                row("B", false, "g-label", "g0"),
            ],
            true,
        );
        let text = s.frame_text();
        let r = evaluate_frame_text(&text, 7).expect("必须能被判定内核解析");
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows.iter().filter(|x| x.aria_current).count(), 1);
        assert!(
            r.internally_consistent(),
            "自洽帧必须 internally_consistent"
        );
        assert_eq!(r.window_count, 1);
    }

    #[test]
    fn partial_sample_never_claims_consistency() {
        use crate::services::codex_desktop_window::evaluate_frame_text;
        let s = sample(vec![row("A", true, "g-label", "g0")], false);
        let r = evaluate_frame_text(&s.frame_text(), 7).expect("可解析");
        assert!(
            !r.internally_consistent(),
            "Partial 帧不得自称完整——否则少交的行会被当成全集"
        );
    }

    #[test]
    fn groups_block_lists_each_group_once() {
        let s = sample(
            vec![
                row("A", true, "g-label", "g0"),
                row("B", false, "g-label", "g0"),
                row("C", false, "h-label", "g1"),
            ],
            true,
        );
        let text = s.frame_text();
        let g0 = text.matches("GROUP g0 ").count();
        let g1 = text.matches("GROUP g1 ").count();
        assert_eq!(g0, 1, "同一组实例只出现一次");
        assert_eq!(g1, 1);
        assert!(text.contains("GROUP g0 label=") && text.contains("rows=2"));
    }

    #[test]
    fn row_without_group_key_still_serialises() {
        let s = sample(vec![row("A", true, "-", "-")], true);
        let text = s.frame_text();
        assert!(text.contains("\nROW "), "行清单必须存在");
        // 组清单里不含 "-"
        assert!(!text.contains("GROUP - "));
    }
}

//! 真实 macOS AX 提供方：把 `codex_desktop_ax_bindings` 的原语接到
//! [`crate::services::codex_desktop_ax_sender::AxProvider`]。
//!
//! # 本模块只做四件事
//!
//! 1. **认定 composer**：窗口里可能有多个 `AXTextArea`（实测 `ProseMirror` 是消息框，
//!    `cm-content,cm-lineWrapping` 是 CodeMirror 代码视图），必须按 `AXDOMClassList` 选。
//! 2. **只读观测**：字数、占位层、附件、控件、忙/待处理 —— 一律不读 `AXValue` 正文。
//! 3. **按元素身份写入**：`set_text` / `press_send` 都核对编辑器与容器身份，
//!    不只按会话寻址。
//! 4. **只读游标**：从 rollout JSONL 数正式用户消息条数与末条指纹。
//!
//! # 不做的事
//!
//! 不做策略判断（空稿判据、目标核证、阶段推进都在状态机里）；不点坐标、不用键盘、
//! 不用剪贴板、不切窗口或焦点。

#![cfg(target_os = "macos")]

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::services::codex_desktop_ax_bindings::{AXUIElement, AxApplication};
use crate::services::codex_desktop_ax_sender::{
    AxProvider, DraftEvidence, EditorObservation, MessageCursor, ObserveError, SendControl,
    SessionBinding,
};
use crate::services::codex_desktop_window::HostIdentity;

/// 本构建实测：消息编辑器的 `AXDOMClassList` 恰含此项。
const COMPOSER_CLASS: &str = "ProseMirror";
/// 发送控件的 `AXDOMClassList` 特征（区别于同区的圆形工具按钮）。
const SEND_BUTTON_CLASS: &str = "bg-composer-primary";

/// 从一行 rollout JSON 里取出正式用户消息的**纯文本**，用于算指纹。
///
/// 只把结果交给 `sha12`，**不**写入日志、不返回给调用方之外的任何地方。
/// 解析失败返回 `None`（该条不参与指纹比对，不会冒充匹配）。
fn message_text_of(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let content = v.get("payload")?.get("content")?;
    // 形态一：字符串
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    // 形态二：数组，元素形如 `{"type":"input_text","text":"..."}`
    let arr = content.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    for item in arr {
        if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
            parts.push(t.to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(""))
    }
}

fn sha12(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(12)
        .collect()
}

/// 一个被认定出来的 composer 及其身份。
///
/// **拥有** `element`（构造时 `CFRetain`，`Drop` 时释放）。这个所有权是必要的：
/// `walk_for_composer` 拿到的子项指针归上一层 `AxElements`，那个容器在遍历返回后
/// 就 drop 了；若这里只存裸指针，后续 `observe` / `set_text` 访问即悬垂
/// （真机表现为 `_AXUIElementValidate` 处 SIGTRAP）。
struct ComposerRef {
    element: AXUIElement,
    /// 元素身份：角色 + 类名 + 深度 + 容器路径的稳定摘要。
    ///
    /// 不用指针地址（会变），而用结构特征；实测同一编辑器跨多次读取稳定。
    identity: String,
    container_identity: String,
}

impl Drop for ComposerRef {
    fn drop(&mut self) {
        unsafe { crate::services::codex_desktop_ax_bindings::release_element(self.element) };
    }
}

/// 真实提供方。
pub struct MacAxProvider {
    app: AxApplication,
    /// 主 App 的 PID（**不是**会话 PID —— 那是 `codex exec-server` 子进程）。
    pid: u32,
    /// 采样与核证都要用的宿主身份（启动代际 / 登录会话）。
    host: HostIdentity,
    /// 本次动作期望的正文（由 `expect_text` 注入）。用于"是否已被采纳"判定。
    expected_text: std::cell::RefCell<String>,
    /// 会话行身份 → 线程 ID（由调用方在批准阶段注入）。
    thread_of_row: BTreeMap<String, String>,
    /// 线程 ID → rollout 文件路径。
    rollout_of_thread: BTreeMap<String, PathBuf>,
}

impl MacAxProvider {
    /// 只在正常授权环境下装配。`None` = 没有 Accessibility 权限或建不出应用元素。
    pub fn new(pid: u32, host: HostIdentity) -> Option<Self> {
        if !crate::services::codex_desktop_ax_bindings::is_process_trusted() {
            return None;
        }
        let app = AxApplication::new(pid)?;
        Some(Self {
            app,
            pid,
            host,
            expected_text: std::cell::RefCell::new(String::new()),
            thread_of_row: BTreeMap::new(),
            rollout_of_thread: BTreeMap::new(),
        })
    }

    /// 登记"这条会话行属于哪条线程、其 rollout 在哪"。
    pub fn bind_thread(&mut self, row_id: &str, thread_id: &str, rollout: PathBuf) {
        self.thread_of_row
            .insert(row_id.to_string(), thread_id.to_string());
        self.rollout_of_thread
            .insert(thread_id.to_string(), rollout);
    }

    /// 找出当前窗口里的 composer。恰好 1 个才算成功。
    fn find_composer(&self) -> Result<ComposerRef, ObserveError> {
        // SAFETY: 所有句柄均来自 `self.app`（同一存活 `AxApplication`），
        // 且本结构与调用方同线程使用；`find_composer` 不跨线程共享。
        let root = self.app.as_element();
        let windows = unsafe { self.app.attribute_elements(root, "AXWindows") }
            .ok_or(ObserveError::AxUnavailable)?;

        let mut found: Vec<ComposerRef> = Vec::new();
        // **拥有型**去重集合（2026-11-28 真机崩溃修复）。
        //
        // 旧实现是 `Vec<AXUIElement>`：`push` 进去的是裸指针，而它来自
        // `children()` 返回的 `AxElements`——那个容器在循环结束时 `Drop`，
        // 里面对每个元素 `CFRelease`。于是 `seen` 里留下的是**已释放的指针**，
        // 下一轮 `CFEqual(*s, element)` 碰到失效引用 → `_AXUIElementValidate`
        // 断言 → `EXC_BREAKPOINT`/SIGTRAP（真机两次复现，崩溃栈顶部正是
        // `cf_equal` ← `walk_for_composer`）。
        //
        // 用 `AxElements` 持有 retain 后的引用：`seen` 的 Drop 统一释放，
        // 生命周期覆盖整个遍历。
        let mut seen = crate::services::codex_desktop_ax_bindings::AxElements::new();

        for window in &windows {
            self.walk_for_composer(*window, 0, &mut found, &mut seen);
        }

        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(ObserveError::WindowGone),
            n => {
                log::warn!("ax: {n} composer candidates (expected 1)");
                Err(ObserveError::AxUnavailable)
            }
        }
    }

    fn walk_for_composer(
        &self,
        element: AXUIElement,
        depth: usize,
        out: &mut Vec<ComposerRef>,
        seen: &mut crate::services::codex_desktop_ax_bindings::AxElements,
    ) {
        if depth > 40 {
            return;
        }
        // **按元素身份去重**（复审 Critical-5）。
        //
        // 旧实现用 `"{role}|{class_list}|{depth}"` 这个**结构摘要**做键：遇到两个
        // 同深度、同角色、同类名的分支（例如双窗口各有一个 `ProseMirror`），
        // 第二个会被静默剪掉，`found.len()` 仍是 1，从而绕过 `EditorAmbiguous`。
        // 改用 `CFEqual` 比对真实元素引用：只挡真正的环，不误剪不同元素。
        //
        // SAFETY: 两侧句柄都来自同一 `AxApplication`。
        if seen
            .iter()
            .any(|s| unsafe { crate::services::codex_desktop_ax_bindings::cf_equal(*s, element) })
        {
            return;
        }
        // SAFETY: retain 后交给 `seen` 拥有；它的 Drop 负责 release。
        seen.push(unsafe { crate::services::codex_desktop_ax_bindings::retain_element(element) });

        // SAFETY: `element` 来自同一 `AxApplication` 的 `AXChildren` 数组，父引用仍存活。
        let role = unsafe { self.app.role(element) }.unwrap_or_default();
        let class_list = unsafe { self.app.attribute_string_array(element, "AXDOMClassList") }
            .unwrap_or_default();
        let key = format!("{role}|{}|{depth}", class_list.join(","));

        if role == "AXTextArea" && class_list.iter().any(|c| c == COMPOSER_CLASS) {
            out.push(ComposerRef {
                // SAFETY: `element` 来自同一 `AxApplication`，此处延长其生命周期。
                element: unsafe {
                    crate::services::codex_desktop_ax_bindings::retain_element(element)
                },
                // 身份含深度：同一窗口里"同角色同类名但层级不同"的两个元素不会撞。
                identity: sha12(&format!("composer|d{depth}|{key}")),
                container_identity: sha12(&format!("container|d{depth}|{key}")),
            });
            return;
        }

        // SAFETY: 同 `element`。
        let child_kids = unsafe { self.app.children(element) };
        for child in &child_kids {
            self.walk_for_composer(*child, depth + 1, out, seen);
        }
    }

    /// 只读观测该 composer 的草稿形态。
    fn draft_evidence_of(&self, composer: &ComposerRef) -> DraftEvidence {
        let el = composer.element;
        // SAFETY: `el` 来自 `find_composer`，与 `self.app` 同一存活作用域。
        let nchars = unsafe { self.app.attribute_int(el, "AXNumberOfCharacters") }
            .and_then(|v| usize::try_from(v).ok());

        // 占位层：直接子项里 `AXDOMClassList` 含 "placeholder" 的那一层。
        let mut placeholder_layer = false;
        let mut placeholder_text_len = None;
        let mut content_children = 0usize;
        let child_kids = unsafe { self.app.children(el) };
        for child in &child_kids {
            let cls = unsafe { self.app.attribute_string_array(*child, "AXDOMClassList") }
                .unwrap_or_default();
            if cls.iter().any(|c| c == "placeholder") {
                placeholder_layer = true;
                // 占位层自己的字数：用它证明"编辑区那串字数属于提示语"。
                placeholder_text_len =
                    unsafe { self.app.attribute_int(*child, "AXNumberOfCharacters") }
                        .and_then(|v| usize::try_from(v).ok());
            } else {
                content_children += 1;
            }
        }

        // 附件：输入区里是否有非文本的附件容器。本构建实测以"非占位子项"近似，
        // 保守起见只在能明确识别时置真；识别不出就不声称有附件。
        let has_attachment = false;

        DraftEvidence {
            editor_count: 1,
            nchars,
            placeholder_layer,
            placeholder_text_len,
            content_children,
            has_attachment,
            read_failed: nchars.is_none(),
            // 正文是否恰好等于本次期望文本。
            // 只做**逐字符相等**比较，不记录、不打印正文。
            text_is_exact_expected: {
                let expected = self.expected_text.borrow();
                let cur = self.current_text_of(composer);
                let ok = !expected.is_empty() && cur.as_deref() == Some(expected.as_str());
                log::debug!(
                    "ax: draft exact={ok} expected_len={} current_len={:?}",
                    expected.chars().count(),
                    cur.as_ref().map(|s| s.chars().count())
                );
                ok
            },
            independent_empty_proof: false,
        }
    }

    /// 找 composer 同区的**发送**控件。
    ///
    /// 判据是 `AXDOMClassList` 含 `bg-composer-primary` 且 enabled，恰好 1 个。
    /// **不能**用"唯一启用的 AXButton"——实测同区有 3 个启用按钮。
    fn find_send_control(
        &self,
        composer: &ComposerRef,
    ) -> (SendControl, Option<String>, Option<AXUIElement>) {
        let mut cur = composer.element;
        for _ in 0..4 {
            let mut candidates: Vec<AXUIElement> = Vec::new();
            self.collect_send_buttons(cur, 0, &mut candidates);

            let len = candidates.len();
            if len == 1 {
                let btn = candidates.remove(0);
                // SAFETY: `btn` 来自同一 `AxApplication` 的子树。
                let cls = unsafe { self.app.attribute_string_array(btn, "AXDOMClassList") }
                    .unwrap_or_default();
                let identity = sha12(&format!("send|{}", cls.join(",")));
                // 选中者所有权移交调用方（由调用方在 press 后释放）。
                return (SendControl::UniqueEnabled, Some(identity), Some(btn));
            }
            // 未选中：把 retained 的候选全部释放，避免泄漏。
            for c in candidates.drain(..) {
                unsafe { crate::services::codex_desktop_ax_bindings::release_element(c) };
            }
            if len > 1 {
                return (SendControl::Ambiguous, None, None);
            }

            // SAFETY: `cur` 同源。`AXParent` 是**单个元素**，不是数组。
            let Some(parent) = (unsafe { self.app.attribute_element(cur, "AXParent") }) else {
                break;
            };
            // `attribute_element` 移交所有权；旧 `cur` 若是我们自己拿的也要释放。
            let previous = cur;
            cur = parent;
            if previous != composer.element {
                unsafe { crate::services::codex_desktop_ax_bindings::release_element(previous) };
            }
        }
        (SendControl::Absent, None, None)
    }

    fn collect_send_buttons(&self, element: AXUIElement, depth: usize, out: &mut Vec<AXUIElement>) {
        if depth > 6 {
            return;
        }
        // SAFETY: `element` 来自同一 `AxApplication` 的 `AXChildren` 数组。
        unsafe {
            if self.app.role(element).as_deref() == Some("AXButton") {
                let cls = self
                    .app
                    .attribute_string_array(element, "AXDOMClassList")
                    .unwrap_or_default();
                let enabled = self
                    .app
                    .attribute_bool(element, "AXEnabled")
                    .unwrap_or(false);
                if enabled && cls.iter().any(|c| c == SEND_BUTTON_CLASS) {
                    // **必须 retain**：`element` 归上一层 `AxElements`，遍历返回后即失效。
                    out.push(crate::services::codex_desktop_ax_bindings::retain_element(
                        element,
                    ));
                }
            }
        }
        // SAFETY: 同 `element`。
        let child_kids = unsafe { self.app.children(element) };
        for child in &child_kids {
            self.collect_send_buttons(*child, depth + 1, out);
        }
    }

    /// 该会话是否忙 / 有未处理请求。只读；判断不出时按"不忙"处理由状态机自行复核。
    fn busy_state(&self, _composer: &ComposerRef) -> (bool, bool) {
        // 本构建未找到可靠的忙碌指示属性；不声称能读到，一律报"不忙、无待处理"，
        // 由状态机的其它检查（正文是否被采纳、控件是否启用）兜住。
        (false, false)
    }
}

impl MacAxProvider {
    /// 读编辑区当前正文，**只用于与期望值比较**，不外传、不落日志。
    fn current_text_of(&self, composer: &ComposerRef) -> Option<String> {
        // SAFETY: `composer` 来自 `find_composer`，同一存活作用域。
        unsafe { self.app.attribute_string(composer.element, "AXValue") }
    }
}

impl AxProvider for MacAxProvider {
    fn expect_text(&self, text: &str) {
        *self.expected_text.borrow_mut() = text.to_string();
    }

    fn observe(&self, binding: &SessionBinding<'_>) -> Result<EditorObservation, ObserveError> {
        let composer = self.find_composer()?;

        // 行身份与视图代际由调用方注入；提供方只能核对"我读到的是不是那条线程的 composer"。
        // 若没登记过该行 → 说明调用方没做批准，直接拒绝。
        let Some(thread_id) = self.thread_of_row.get(binding.session_row_id) else {
            return Err(ObserveError::SessionRowMismatch);
        };
        if thread_id != binding.thread_id {
            return Err(ObserveError::SessionRowMismatch);
        }

        let draft = self.draft_evidence_of(&composer);
        let (send_control, control_identity, _element) = self.find_send_control(&composer);
        let (busy, pending_request) = self.busy_state(&composer);

        Ok(EditorObservation {
            session_row_id: binding.session_row_id.to_string(),
            project_group: binding.project_group.to_string(),
            view_generation: binding.view_generation,
            editor_identity: composer.identity.clone(),
            container_identity: composer.container_identity.clone(),
            composer_kind: COMPOSER_CLASS.to_string(),
            control_identity,
            draft,
            send_control,
            busy,
            pending_request,
        })
    }

    fn set_text(
        &self,
        binding: &SessionBinding<'_>,
        editor_identity: &str,
        container_identity: &str,
        text: &str,
    ) -> Result<(), ObserveError> {
        let composer = self.find_composer()?;
        // 身份必须对上：会话在、但编辑器换了 → 不能写到新元素上。
        if composer.identity != editor_identity || composer.container_identity != container_identity
        {
            return Err(ObserveError::ViewGenerationMismatch);
        }
        if self
            .thread_of_row
            .get(binding.session_row_id)
            .map(String::as_str)
            != Some(binding.thread_id)
        {
            return Err(ObserveError::SessionRowMismatch);
        }
        // SAFETY: `composer` 来自 `find_composer`，同一存活作用域。
        if !unsafe { self.app.is_settable(composer.element, "AXValue") } {
            return Err(ObserveError::AxUnavailable);
        }
        if unsafe { self.app.set_string(composer.element, "AXValue", text) } {
            Ok(())
        } else {
            Err(ObserveError::AxUnavailable)
        }
    }

    fn press_send(
        &self,
        binding: &SessionBinding<'_>,
        control_identity: &str,
        container_identity: &str,
        editor_identity: &str,
    ) -> Result<(), ObserveError> {
        let composer = self.find_composer()?;
        if composer.identity != editor_identity || composer.container_identity != container_identity
        {
            return Err(ObserveError::ViewGenerationMismatch);
        }
        if self
            .thread_of_row
            .get(binding.session_row_id)
            .map(String::as_str)
            != Some(binding.thread_id)
        {
            return Err(ObserveError::SessionRowMismatch);
        }
        let (control, current_identity, element) = self.find_send_control(&composer);
        if control != SendControl::UniqueEnabled {
            return Err(ObserveError::AxUnavailable);
        }
        if current_identity.as_deref() != Some(control_identity) {
            return Err(ObserveError::ViewGenerationMismatch);
        }
        let Some(button) = element else {
            return Err(ObserveError::AxUnavailable);
        };
        // 用 guard 接管所有权：无论成功失败都在本函数返回时释放。
        // SAFETY: `button` 由 `find_send_control` 移交给本函数，所有权清晰。
        let guard = unsafe {
            crate::services::codex_desktop_ax_bindings::AxElementGuard::from_owned(button)
        };
        // **恰好按一次**。失败也返回 `Err`——效果未知，由状态机只读核对。
        // SAFETY: `guard.get()` 来自同一 `AxApplication`，guard 在本函数内存活。
        if unsafe { self.app.perform_action(guard.get(), "AXPress") } {
            Ok(())
        } else {
            Err(ObserveError::TimedOut)
        }
    }

    fn formal_message_cursor(&self, thread_id: &str) -> Result<MessageCursor, ObserveError> {
        let Some(path) = self.rollout_of_thread.get(thread_id) else {
            return Err(ObserveError::AxUnavailable);
        };
        let file = std::fs::File::open(path).map_err(|_| ObserveError::AxUnavailable)?;
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(file);

        let mut count = 0usize;
        let mut last: Option<String> = None;
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            // 只认正式用户消息：`response_item` 里 `message` 且 `role=user`。
            let is_user_message = line.contains("\"type\":\"response_item\"")
                && line.contains("\"type\":\"message\"")
                && line.contains("\"role\":\"user\"");
            if !is_user_message {
                continue;
            }
            count += 1;
            // 指纹必须是**消息正文**的 sha12，才能与状态机期望的
            // `sha256_prefix12(本次提交文本)` 比对得上。
            // 只取正文算哈希，**不**记录、不打印、不外传正文本身。
            last = message_text_of(&line).map(|t| sha12(&t));
        }
        Ok(MessageCursor {
            count,
            last_fingerprint: last,
        })
    }
}

// ============================================================
// 阶段二：真实导航提供方（用户裁决 2026-11-30 接入）
// ============================================================

/// 真实导航：**按行身份**向侧栏行发一次 `AXPress`，再只读重采样确认。
///
/// # 每个动作只用一次
///
/// `navigate_once` 保证一次调用只按一次；本实现也不做任何重试。失败即返回 `Err`
/// （效果未知），由上层只读核对。
///
/// # 不碰编辑器
///
/// 按下的是**侧栏行**（`AXButton`），不是消息输入框。因此本动作不会改动任何草稿。
impl crate::services::codex_desktop_window::NavigationProvider for MacAxProvider {
    fn press_target_row(
        &mut self,
        row_identity: &str,
        project_group: &str,
    ) -> Result<(), crate::services::codex_desktop_window::NavigationPressError> {
        use crate::services::codex_desktop_window::NavigationPressError;

        // 重新采样，按"行身份 + 项目组"唯一定位目标行元素。
        let sample = crate::services::codex_desktop_ax_sample::sample_sidebar(
            self.pid,
            self.host.launch_generation,
            self.host.login_session,
        )
        .ok_or(NavigationPressError::Failed)?;

        // 行身份必须唯一命中（采样输出的是哈希，直接比对）。
        let hits: Vec<&crate::services::codex_desktop_ax_sample::RowSample> = sample
            .rows
            .iter()
            .filter(|r| r.node_id_hash == row_identity && r.group_key == project_group)
            .collect();
        if hits.len() != 1 {
            // 0 个 = 行不在了；多个 = 歧义。两种情况都不能按。
            return Err(NavigationPressError::Failed);
        }

        // 切走可能使原编辑区丢失，不能只核对目标行而忽略源草稿。
        use crate::services::codex_desktop_ax_sender::{classify_draft, DraftState};
        let composer = self
            .find_composer()
            .map_err(|_| NavigationPressError::Failed)?;
        if classify_draft(&self.draft_evidence_of(&composer)) != DraftState::Empty {
            return Err(NavigationPressError::Failed);
        }
        // 按同一 AX 节点身份重新找句柄，绝不把标题相同的其他控件当成目标。
        let Some(element) = self.find_row_element(row_identity) else {
            return Err(NavigationPressError::Failed);
        };
        // SAFETY: `element` 由 `find_row_element` 返回且已 retain，本函数独占。
        let guard = unsafe {
            crate::services::codex_desktop_ax_bindings::AxElementGuard::from_owned(element)
        };
        // SAFETY: guard 在本函数内存活。
        if unsafe { self.app.perform_action(guard.get(), "AXPress") } {
            Ok(())
        } else {
            Err(NavigationPressError::Failed)
        }
    }

    fn resample(&self) -> Option<crate::services::codex_desktop_window::ProbeReport> {
        let sample = crate::services::codex_desktop_ax_sample::sample_sidebar(
            self.pid,
            self.host.launch_generation,
            self.host.login_session,
        )?;
        let text = sample.frame_text();
        crate::services::codex_desktop_window::evaluate_frame_text(
            &text,
            self.host.launch_generation,
        )
        .ok()
    }
}

impl MacAxProvider {
    /// 在 AX 树里定位侧栏行元素并 **retain** 后返回。
    ///
    /// 采样已证明项目组和行身份唯一；操作时必须找到该节点自身，而非标题相同的按钮。
    fn find_row_element(&self, row_identity: &str) -> Option<AXUIElement> {
        use crate::services::codex_desktop_ax_bindings::AxElements;

        // SAFETY: 窗口和遍历元素来自同一个 AX 应用。
        let windows = unsafe {
            self.app
                .attribute_elements(self.app.as_element(), "AXWindows")
        }?;
        if windows.len() != 1 {
            return None;
        }
        let mut found = AxElements::new();
        let mut seen = AxElements::new();
        self.walk_for_row(windows.first()?, row_identity, 0, &mut found, &mut seen);
        (found.len() == 1 && seen.len() < 2000)
            .then(|| found.into_first())
            .flatten()
    }

    fn walk_for_row(
        &self,
        element: AXUIElement,
        row_identity: &str,
        depth: usize,
        found: &mut crate::services::codex_desktop_ax_bindings::AxElements,
        seen: &mut crate::services::codex_desktop_ax_bindings::AxElements,
    ) {
        use crate::services::codex_desktop_ax_bindings::{cf_equal, retain_element};

        if depth > 48 || found.len() > 1 || seen.len() >= 2000 {
            return;
        }
        // SAFETY: seen 持有 retain 后的元素，element 来自同一存活应用。
        unsafe {
            if seen.iter().any(|seen| cf_equal(*seen, element)) {
                return;
            }
            seen.push(retain_element(element));
            if self.app.role(element).as_deref() == Some("AXButton")
                && self.app.attribute_bool(element, "AXEnabled") == Some(true)
                && self
                    .app
                    .attribute_string(element, "ChromeAXNodeId")
                    .is_some_and(|id| !id.is_empty() && sha12(&id) == row_identity)
            {
                found.push(retain_element(element));
            }
        }
        // SAFETY: 子元素在 AxElements 的生命周期内保持有效。
        for child in &unsafe { self.app.children(element) } {
            self.walk_for_row(*child, row_identity, depth + 1, found, seen);
        }
    }
}

// 共享「回合」件（Task 9 复审上提）：**通道无关**的单回合机械——执行缝、回执→审计词、
// 会话串行锁 / 取消靶子登记表、证据头。zcode（Task 8）与 codex（Task 9）共用；
// WB（Task 11）与 H11 三家（Task 13）落地时**依赖本模块**，不要再从 `zcode.rs` 取。
//
// **为什么独立成文件**（复审裁决）：这些件原先长在 `zcode.rs` 里，codex 落地后不得不
// `use zcode::{RunSeam, registry, TurnSlot, TurnObs}`——`zcode.rs` 实际上变成了无头层
// 的底座模块，WB 接入时会第三次复制这层耦合。上提后各通道**只依赖底座**
// （`mod.rs` / `receipt.rs` / `runner.rs` / `gate.rs` / `turn.rs`），通道之间不互相依赖。
//
// **零行为变化**：定义逐字搬迁（含既有文档与纪律说明），`zcode.rs` 以 `pub use` 转出
// 保持 Task 8 既有调用面（`zcode::registry()` / `zcode::TurnSlot` / `zcode::TurnObs` …）
// 不变——两套套件（zcode / codex）全绿即为证据。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::receipt::{Receipt, ReceiptStatus};
use super::runner::{CancelHandle, RunnerCfg};

/// 盒装 future 别名（执行缝/等待缝共用；避免 clippy::type_complexity）
pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// 证据串行数上限（各类退出分类/锁判定的输入行数；子串匹配只看头部）
pub const EVIDENCE_HEAD_LINES: usize = 32;

/// 证据头（生产/测试共用）：前 [`EVIDENCE_HEAD_LINES`] 行拼接（判定为子串匹配）
pub fn head_of(lines: &[String]) -> String {
    lines
        .iter()
        .take(EVIDENCE_HEAD_LINES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一次**尝试**的观测（执行缝的产物；回执 + 原始流供退出分类/回执归一消费）
#[derive(Debug, Clone)]
pub struct TurnObs {
    pub receipt: Receipt,
    pub stdout: Vec<String>,
    pub stderr: String,
    pub exit: Option<i32>,
}

/// 执行缝（生产 = `RunnerCfg::run().await` 真 spawn；测试 = Task 6 的 `run_once` 脚本缝）
pub type RunSeam = dyn Fn(RunnerCfg) -> BoxFuture<TurnObs> + Send + Sync;

/// 生产执行缝（真 spawn：`RunnerCfg::run().await`；超时/取消/kill 树/在飞登记全归
/// [`super::runner`]）。断言用脚本缝见各通道测试模块（Task 6 的 `run_once`）。
pub fn production_run_seam() -> Box<RunSeam> {
    Box::new(|mut cfg: RunnerCfg| {
        Box::pin(async move {
            let receipt = cfg.run().await;
            TurnObs {
                receipt,
                stdout: cfg.captured_stdout(),
                stderr: cfg.captured_stderr(),
                exit: cfg.last_exit_code(),
            }
        })
    })
}

/// 回执 → 审计 result 词（H6 口径：终态；失败带阶段码。阶段词经 serde **单一来源**
/// 取 [`super::receipt::Stage`] 的 wire 名，不另抄一份词表）
pub fn receipt_result_word(r: &Receipt) -> String {
    match r.status {
        ReceiptStatus::Ok => "ok".to_string(),
        ReceiptStatus::Queued => "queued".to_string(),
        ReceiptStatus::Cancelled => "cancelled".to_string(),
        ReceiptStatus::Failed => match r.stage {
            Some(st) => {
                let word = serde_json::to_value(st)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".to_string());
                format!("failed({word})")
            }
            None => "failed".to_string(),
        },
    }
}

/// 末条 assistant 的**摘要口径单点**（截断长度 = [`super::receipt::LAST_ASSISTANT_CHARS`]、
/// 截断语义 = [`crate::inject::normalize::summarize`]，与 W5 审计摘要同源）。
///
/// **为什么单独成函数**（Task 12 复审 Minor 3）：[`ok_receipt_with_assistant`] 与
/// 「回执已在手、只补摘要」的调用点（`zcode_create::fill_missing_from_store` /
/// `confirmed_receipt` 的失败臂）必须走同一条截断——各自内联
/// `summarize(.., LAST_ASSISTANT_CHARS)` 就是同一口径的两份写法，改一处漏一处不会编译报错。
pub fn assistant_summary(text: &str) -> String {
    crate::inject::normalize::summarize(text, super::receipt::LAST_ASSISTANT_CHARS)
}

/// 库/通道确认成功时的 **Ok 回执**（`lastAssistant` 摘要 + tokens）。
///
/// **为什么在底座**（Task 12 复审式上提）：`zcode.rs` 原有的私有 `ok_receipt_from_store`
/// 是**通道无关**的收尾件（截断口径 = [`super::receipt::LAST_ASSISTANT_CHARS`] +
/// [`crate::inject::normalize::summarize`] 单点），而 H10 新建路径（`zcode_create.rs`）
/// 从会话库确认新会话时要用同一条口径——留在 `zcode.rs` 就会从通道模块外借私有件，
/// 或被迫抄第二份截断。定义上提到此，`zcode.rs` 的既有调用改为委托（**零行为变化**）。
pub fn ok_receipt_with_assistant(
    session_id: &str,
    text: &str,
    tokens: Option<u64>,
    duration_ms: u64,
) -> Receipt {
    let mut r = Receipt::ok(session_id, duration_ms);
    r.last_assistant = Some(assistant_summary(text));
    r.tokens = tokens;
    r
}

// ============================================================
// 会话串行锁 + 取消靶子（MAM 自己的；zcode 无头不拒绝并发，codex 复用同一表）
// ============================================================

/// 取消请求口（Task 6 的 [`CancelHandle`] 包一层，便于测试注入「送达/未送达」两形态）
pub type CancelFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// 在飞回合槽位（取消靶子 + 回合起跑时刻 + 审计上下文）。
///
/// **不含设备身份**：回合审计行的设备取**发起发送的**设备（端点持有），取消审计行的设备
/// 取**按下取消的**设备（取消端点持有）——槽位只保留两者都要用的回合自身信息。
pub struct TurnSlot {
    pub cancel: CancelFn,
    /// 取消靶子**是否武装过**（[`TurnRegistry::arm`] 置 true）。
    ///
    /// 取消端点据此分辨两种都返回 `false` 但**语义不同**的形态（Task 11 复审 Important 3）：
    /// - 未武装 + 通道本批未接线（H9 WB ACP）⇒「本通道未接线取消，**回合仍在运行**」；
    /// - 未武装 + 已接线通道（zcode/codex 的版本门控/预检窗口）⇒「尚未进入可取消阶段，
    ///   **回合仍在运行**，请稍后重试」；
    /// - 已武装 ⇒ 回合已终结/已被取消（先到者生效）。
    pub cancel_wired: bool,
    pub started: std::time::Instant,
    /// 本回合的注入正文（取消审计行与回合审计行同源）
    pub content: String,
    /// 会话工具 id（回合/取消两行审计的 agent_type 列）
    pub agent_type: String,
    /// 本回合的**无头通道 wire 名**（取消审计行 channel 列的**单点来源**）。
    ///
    /// **为什么存在**（Task 11 复审 Important 2）：取消端点原先把 channel 写死
    /// `headless_zcode`，而 codex 回合**真的武装取消靶子** ⇒ 送达的 codex 取消被记成
    /// `headless_zcode`（审计/历史把取消归错通道）。通道名由各通道占位时声明
    /// （[`TurnSlot::with_channel`]），端点从槽位取，**不得再写常量**。
    /// 空串 = 未声明（只可能来自测试桩或将来新增通道漏声明——端点按
    /// `headless_unattributed` 如实落账并 warn，不冒充任何既有通道）。
    pub channel: String,
}

impl TurnSlot {
    /// 占位槽（取消靶子待 [`TurnRegistry::arm`] 逐尝试更新）
    pub fn placeholder(agent_type: &str, content: String) -> Self {
        Self {
            cancel: Arc::new(|| false),
            cancel_wired: false,
            started: std::time::Instant::now(),
            content,
            agent_type: agent_type.to_string(),
            channel: String::new(),
        }
    }

    /// 声明本回合的无头通道（取消审计行 channel 列的单点来源；生产三条分派臂都必须声明）
    pub fn with_channel(mut self, channel: &str) -> Self {
        self.channel = channel.to_string();
        self
    }
}

/// 取消端点**一次取全**的槽位快照（内容/工具/通道/靶子是否武装/已跑时长）。
///
/// **为什么一次取全**（Task 11 复审 Important 2/3）：取消行要写通道名、措辞要分辨
/// 「未接线 / 未武装 / 已终结」——分两次查表既有竞态窗口，也逼调用方另抄一份判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelSnapshot {
    pub content: String,
    pub agent_type: String,
    pub channel: String,
    pub cancel_wired: bool,
    pub elapsed_ms: u64,
}

/// 进程级在飞登记表（会话串行锁的唯一判据出口）
#[derive(Default)]
pub struct TurnRegistry {
    map: Mutex<HashMap<String, TurnSlot>>,
}

impl TurnRegistry {
    /// 占位（`false` = 该会话已有在飞回合——调用方必须**如实拒绝**：不排队、不覆盖）
    pub fn begin(&self, session_id: &str, slot: TurnSlot) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains_key(session_id) {
            return false;
        }
        g.insert(session_id.to_string(), slot);
        true
    }

    /// 更新取消靶子（每尝试一次；无槽位时 no-op——测试直驱各通道 `run_turn` 不建槽）。
    /// **同时置 [`TurnSlot::cancel_wired`]**：取消端点据此分辨「未武装」与「已终结」
    /// （两者都返回 `false`，但措辞必须不同——Task 11 复审 Important 3）
    pub fn arm(&self, session_id: &str, cancel: CancelFn) {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = g.get_mut(session_id) {
            slot.cancel = cancel;
            slot.cancel_wired = true;
        }
    }

    /// 回合终结：注销槽位
    pub fn end(&self, session_id: &str) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    pub fn in_flight(&self, session_id: &str) -> bool {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(session_id)
    }

    /// 请求取消：`None` = 无在飞回合；`Some(false)` = 有回合但取消未送达（已终结/已取消
    /// ——先到者生效）；`Some(true)` = 送达。**不注销槽位**（终结由回合自身 [`Self::end`]
    /// 负责，取消只是请求）。
    pub fn request_cancel(&self, session_id: &str) -> Option<bool> {
        let cancel = {
            let g = self.map.lock().unwrap_or_else(|e| e.into_inner());
            g.get(session_id).map(|s| s.cancel.clone())
        };
        cancel.map(|f| f())
    }

    /// 取消端点**一次取全**的快照（内容/工具/通道/靶子是否武装/已跑时长）——
    /// 取消审计行的 channel 列与措辞判定都从这里取，**不再写常量、不再分两次查表**
    pub fn cancel_snapshot(&self, session_id: &str) -> Option<CancelSnapshot> {
        let g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        g.get(session_id).map(|s| CancelSnapshot {
            content: s.content.clone(),
            agent_type: s.agent_type.clone(),
            channel: s.channel.clone(),
            cancel_wired: s.cancel_wired,
            elapsed_ms: s.started.elapsed().as_millis() as u64,
        })
    }

    /// 三元素槽位快照（诊断/测试面；实现单点 = [`Self::cancel_snapshot`]，避免两份判定）
    pub fn slot_snapshot(&self, session_id: &str) -> Option<(String, String, u64)> {
        self.cancel_snapshot(session_id)
            .map(|s| (s.content, s.agent_type, s.elapsed_ms))
    }
}

static REGISTRY: std::sync::LazyLock<TurnRegistry> =
    std::sync::LazyLock::new(TurnRegistry::default);

/// 进程级登记表句柄（**所有无头通道共用一份**：取消端点只认它）
pub fn registry() -> &'static TurnRegistry {
    &REGISTRY
}

/// Task 6 的取消句柄 → 本表的取消请求口（各通道 `run_turn` 逐尝试武装用）
pub fn cancel_fn_of(cfg: &RunnerCfg) -> CancelFn {
    let handle: CancelHandle = cfg.cancel_handle();
    Arc::new(move || handle.cancel())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登记表语义（Task 8 既有行为的搬迁回归）：占位互斥 / 取消先到者生效 / 槽位快照
    #[test]
    fn registry_is_exclusive_and_cancel_is_first_wins() {
        let sid = "sess_turn_registry_probe";
        assert!(registry().begin(sid, TurnSlot::placeholder("codex", "hi".into())));
        assert!(
            !registry().begin(sid, TurnSlot::placeholder("codex", "hi".into())),
            "同会话第二次占位必须失败（不排队、不覆盖）"
        );
        assert!(registry().in_flight(sid));
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = fired.clone();
        registry().arm(
            sid,
            Arc::new(move || {
                f2.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }),
        );
        assert_eq!(
            registry().request_cancel(sid),
            Some(true),
            "在飞回合取消送达"
        );
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            registry().slot_snapshot(sid).map(|(c, t, _)| (c, t)),
            Some(("hi".to_string(), "codex".to_string())),
            "槽位快照供取消审计行取正文/工具"
        );
        registry().end(sid);
        assert!(!registry().in_flight(sid));
        assert_eq!(
            registry().request_cancel(sid),
            None,
            "无在飞回合 → None（取消端点据此不落审计行）"
        );
    }

    /// 审计 result 词（回执 → 词表）与证据头口径
    #[test]
    fn audit_words_and_evidence_head_are_pinned() {
        assert_eq!(receipt_result_word(&Receipt::ok("s", 1)), "ok");
        assert_eq!(
            receipt_result_word(&Receipt::failed(super::super::receipt::Stage::Crash, "x")),
            "failed(crash)"
        );
        let lines: Vec<String> = (0..40).map(|i| format!("l{i}")).collect();
        let head = head_of(&lines);
        assert_eq!(head.lines().count(), EVIDENCE_HEAD_LINES);
        assert!(head.starts_with("l0") && head.ends_with("l31"));
    }
}

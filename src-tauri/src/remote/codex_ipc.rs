// Codex Desktop IPC 远程控制接线（计划 Task 8）：`CodexControlHub` 字段束 + 认证后的
// HTTP/SSE 端点 + 审计与生命周期装配（S2 起无兜底 bridge——Desktop 会话 NotSent 终态）。
//
// 结构纪律（计划「文件结构与公共接口总约定」）：
// - 本模块只做**接线**：路由决策在 `services::codex_control::CodexControlRouter`（Task 7，
//   已测试钉死），协议与投影在 `services::codex_desktop_ipc::DesktopIpcAdapter`（Task 3–6）；
//   HTTP 层不出现 targetClientId/ownerClientId/raw method/permissions 对象，IPC 原始
//   state 不出远程 API（SSE/REST 同一 Tuvis 投影）；
// - 全部端点注册在既有 gated nest（`api_router`）内——设备门禁由内层 layer 结构性覆盖，
//   各端点零鉴权代码；所有响应 no-store；
// - 语义硬约束：未知投递（`unknown_delivery`）是 200 的明确回执，绝不自动重试；
//   **S2 起无兜底**：Desktop 会话 NotSent 即终态回执（route 保持 desktop_ipc，无
//   headless 去向）；H3 与 hub 面无关——headless 只服务 headless 归属会话（api::
//   session_send ④b）；
// - 不在 `get_all_sessions` 扫描里逐条连 IPC：只按请求解析（session → thread 单点）和
//   有界关注集合（adapter 已跟随的 Thread）工作。
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{sse, IntoResponse, Response, Sse};
use axum::Json;
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use crate::database::dao::codex_actions::StopAttemptPhase;
use crate::database::dao::write_audit::record_conn;
use crate::inject::headless::codex::{production_rollout_path, resolve_thread};
use crate::remote::pairing::DeviceStore;
use crate::services::codex_control::{
    reconcile_compat_fence, ActionContext, ActionKind, ActionStore, BarrierRelease,
    CodexControlRouter, CompatFence, Delivery, DesktopDispatch, DispatchOutcome, PendingAction,
    PrioritizeOutcome, RouterOutcome, SendGate,
};
use crate::services::codex_desktop_ipc::adapter::{
    Connector, DesktopIpcAdapter, ProductionConnector,
};
use crate::services::codex_desktop_ipc::mirror::MESSAGES_PAGE_LIMIT;
use crate::services::codex_desktop_ipc::probe::runner::ProbeOutcome;
use crate::services::codex_desktop_ipc::probe::schema_probe::SchemaEvidence;
use crate::services::codex_desktop_ipc::probe::{
    PersistedSchemaEvidence, ProbeLedger, ProbeLedgerEntry,
};
use crate::services::codex_desktop_ipc::profile::{profile_for, ProtocolProfile};
use crate::services::codex_desktop_ipc::types::{
    ActionReceipt, ActionState, CodexActionVerifyView, CodexActionView, CodexQueueView,
    CodexVerifyOutcome, PendingKind, ProtocolError, ThreadKey, ThreadProjection,
};

use super::server::RemoteState;

// ============================================================
// 常量与响应样板
// ============================================================

/// 队列视图的兼容在册阻塞码（计划 Task 8 端点表：fence 在场 → blockedReasonCode）。
pub(crate) const BLOCKED_COMPAT_OUTSTANDING: &str = "compat_outstanding";
/// 原生 Turn 屏障的在册码（2026-10-18 形态适配配套）：route=desktop_ipc 的
/// Accepted 动作在等精确 Turn 终态证据——不是「兼容模式」，UI 分文案。
pub(crate) const BLOCKED_NATIVE_TURN_PENDING: &str = "native_turn_pending";
/// 原生活动回合（2026-10-22 原生回合打断扩展）：无屏障但镜像里有 `inProgress`
/// 回合（桌面手打产生的回合）——打断出口对它同样开放，阀值取镜像活动回合 id。
pub(crate) const BLOCKED_NATIVE_TURN_ACTIVE: &str = "native_turn_active";
/// 原生 Unknown/缺 turnId 保守屏障的在册码（复审 P2-3）：**没有自动恢复路径**
/// （释放门要求精确 turnId 终态证据）——文案必须与「等待后自动继续」区分。
pub(crate) const BLOCKED_NATIVE_DELIVERY_UNKNOWN: &str = "native_delivery_unknown";

/// P8（2026-10-31 用户实机）：`interruptedTurnId:null` 后的**专属终态等待窗口**。
///
/// Desktop 回 null 有两种含义：真换代/无可打断回合，或「停止已受理但状态帧还在
/// 路上」（真机常态延迟）。旧实现只做一次即时检查就判前者，导致「第一次点插队
/// 只停不发、再点一次才成功」。这个窗口给后者留出观察时间——真机被打断的回合
/// 通常 1–2 秒内转终态；窗口耗尽仍不转 Ready 才判真换代。取 3 秒：比一次即时
/// 检查宽裕，又不至于让真换代把整条 30 秒前台预算耗光（用户建议 5–10 秒偏长，
/// 会造成插队按钮长时间无响应）。
pub(crate) const STOP_SETTLE_WINDOW_MS: u64 = 3_000;

/// P10（2026-10-31 用户追加要求：「兜底 3 秒之后发现还是没发出去，再补一个」）：
/// 第一段短窗未等到终态时的**追加宽限**。覆盖「桌面停得更慢」的长尾，避免用户
/// 必须再点一次插队。两段合计 7 秒，仍远小于 30 秒前台预算；且都按剩余预算封顶
/// （S3：子步骤不重置预算），真换代场景不会吃光整条预算。
pub(crate) const STOP_SETTLE_RETRY_WINDOW_MS: u64 = 4_000;

/// P10：把「本段等待毫秒数」转成受前台预算封顶的绝对截止——即使调用方给的是
/// 整条预算，也只会等到 `want_ms` 与剩余预算的较小者。
fn settle_window(
    front: crate::services::codex_desktop_ipc::budget::Deadline,
    want_ms: u64,
) -> crate::services::codex_desktop_ipc::budget::Deadline {
    let ms = front
        .remaining()
        .map(|r| (r.as_millis() as u64).min(want_ms))
        .unwrap_or(want_ms);
    crate::services::codex_desktop_ipc::budget::Deadline::after_ms(ms)
}

/// 本机 ThreadKey（服务端只构造 local——计划公共类型约定）。
fn local_key(thread_id: &str) -> ThreadKey {
    ThreadKey {
        host_id: "local".into(),
        thread_id: thread_id.to_string(),
    }
}

/// 状态码 + JSON 的统一 no-store 响应（门禁下私有读写路径一律禁中间层缓存）。
fn json_no_store(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response()
}

/// 错误回执（机器码英文——开发者面；reason 可选中文引导，消费方原样渲染）。
fn error_body(status: StatusCode, code: &str, reason: Option<&str>) -> Response {
    let mut v = serde_json::json!({ "error": code });
    if let Some(r) = reason {
        v["reason"] = serde_json::json!(r);
    }
    json_no_store(status, v)
}

pub(crate) fn bad_request() -> Response {
    error_body(StatusCode::BAD_REQUEST, "bad_request", None)
}

pub(crate) fn not_found() -> Response {
    error_body(StatusCode::NOT_FOUND, "no_session", None)
}

pub(crate) fn forbidden_defense() -> Response {
    error_body(StatusCode::FORBIDDEN, "forbidden", None)
}

pub(crate) fn capability_unavailable() -> Response {
    error_body(
        StatusCode::SERVICE_UNAVAILABLE,
        "capability_unavailable",
        None,
    )
}

// ============================================================
// CodexControlHub（RemoteState 的一个字段束）
// ============================================================

/// hub 内部事件（调度循环消费）：`ThreadSettled` = 某 Thread 有动作落账（派发任务
/// 自结算、入队前拒绝等终态写入）→ 接力 flush 下一条。adapter 的投影广播（idle 边沿）
/// 是另一路输入，两路都汇入 [`settle_thread`]。
#[derive(Debug, Clone)]
pub(crate) enum HubEvent {
    ThreadSettled(ThreadKey),
}

/// Codex 控制面字段束：路由器（持久账本 + FIFO + 安全路由）、Desktop IPC 门面、
/// 内部事件通道、生产生命周期句柄。
///
/// **`disabled()` 契约**（既有全部测试 fixture 显式填 disabled hub——编译即暴露）：
/// 一切能力关闭——`send_available()`/`state_available()` 恒 false（closed profile 能力
/// 全 false → 投影恒 `compat_only`）、审批一律拒绝、`start()` 不装配调度循环（不订阅/
/// 不恢复/不调度）。
/// R8 起普通消息**仍经 router**（登记→reserve→NotSent 即终态回执【S2 起无兜底交接】，
/// 账本纪律不因 hub 不可用绕过）。
pub struct CodexControlHub {
    /// 动作账本 + 安全路由（Task 7；`store()` 供动作/队列端点读取）
    pub router: Arc<CodexControlRouter>,
    /// Desktop IPC 门面（Task 6；投影/ensure/start_turn/respond 唯一入口）
    pub adapter: Arc<DesktopIpcAdapter>,
    /// 内部事件（`ThreadSettled` 接力）
    events: tokio::sync::broadcast::Sender<HubEvent>,
    /// R7.3：关注引用闸（与 AdapterDispatch 共享；SSE 详情订阅 +1/结束 -1）
    gate: Arc<FollowGate>,
    /// 生产能力标志：`disabled()` 恒 false——`start()/stop()/send_available()/
    /// state_available()` 全部短路与拒绝
    capable: bool,
    /// 调度循环在跑标志（幂等启停）
    running: AtomicBool,
    /// 调度循环句柄（stop 时 abort；循环只是「接力调度器」——已派发任务在自己的
    /// detached 任务里跑至落账或 unknown，不随本句柄消失）
    sched: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// R7.2：周期对账间隔（默认 30s；测试可收窄——首个 tick 立即触发，兼作 R7.1
    /// 启动恢复：枚举 queued/屏障在册的 thread → ensure 重新 following → 事件驱动
    /// 调度，不打开详情、不手工 flush 也能恢复发送）
    reconcile_period_ms: AtomicU64,
    /// #172/T7：审批回执的「消失确认」窗口（毫秒）。生产 5000（决策后等 request 从新
    /// snapshot 消失）；测试构造默认 0（不等待、不影响既有用例），需要验证超时的用例
    /// 用 [`set_decision_confirm_ms`](Self::set_decision_confirm_ms) 收窄。
    decision_confirm_ms: AtomicU64,
    /// S3：前台编排预算（毫秒，自服务端接收发送请求起）。注参收窄 = 受控时间测试
    /// （生产恒 [`crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS`]）。
    front_budget_ms: AtomicU64,
    /// S4：单次只读核对预算（毫秒，生产恒
    /// [`crate::services::codex_desktop_ipc::budget::VERIFY_BUDGET_MS`]；注参收窄 =
    /// 受控时间测试）。
    verify_budget_ms: AtomicU64,
    /// N07：本 hub 的**启动/装配身份**（同进程重建 hub 也会换新）；与连接 epoch、
    /// owner generation 一起组成同题占用的 owner 世系。
    owner_boot: String,
}

/// 周期对账默认间隔（R7.2：有界定时器；空待发集合时只有一条 EXISTS 级查询）。
pub(crate) const RECONCILE_PERIOD_MS: u64 = 30_000;

/// #172/T7：审批决策后等「pending 从新快照消失」的窗口（受理 ≠ 生效）。
pub(crate) const DECISION_CONFIRM_MS: u64 = 5_000;

/// 生产 endpoint 解析器（**单一来源**）：控制面装配（[`CodexControlHub::production`]）
/// 与「读链取证查询」（[`read_chain_verified`]）共用同一套解析口径——平台分支、
/// Windows 的 MSIX build 读取都只此一处，杜绝两处口径漂移。
fn production_resolver() -> Arc<dyn crate::services::codex_desktop_ipc::transport::EndpointResolver>
{
    match std::env::consts::OS {
        "macos" => Arc::new(
            crate::services::codex_desktop_ipc::transport::unix::UnixEndpointResolver {
                codex_home: None,
                // Windows 交叉编译下该字段被 #[cfg(unix)] 剔除（本臂不可达但须编译）
                #[cfg(unix)]
                legacy_tmpdir: None,
            },
        ),
        "windows" => Arc::new(
            crate::services::codex_desktop_ipc::transport::windows::WindowsEndpointResolver,
        ),
        _ => Arc::new(NullConnector),
    }
}

/// #172/T8：**该 build 的读链是否已取证**（兼容发送开关的前置判据）。
///
/// 口径（两档都算「已取证」）：
/// * shipped build —— 真机证据已覆盖读链（`profile_for` 命中）；
/// * 未知 build —— 探测账本里该 build 的 `state` 行已是 `probed`/`verified`。
///
/// 为什么查账本而不是读运行期能力位：运行期能力位随连接断开回落 `closed()`，
/// 开关会无缘无故变灰；账本是**持久证据**（跨重启、跨连接都稳定）。
///
/// **只读、无副作用**：不装配控制面、不建连接、不跑 schema 探针；`cli_id` 段用
/// SQL 通配（取它要 spawn 本机 `codex.exe --version`——为一个只读判据不值）。
/// 取不到 build（Desktop 未运行 / 多义 / 平台不支持）→ false（不猜）。
pub fn read_chain_verified(store: &DeviceStore) -> bool {
    let Ok(ep) = production_resolver().resolve() else {
        return false;
    };
    let Some(build) = ep.build.as_deref().map(str::trim).filter(|b| !b.is_empty()) else {
        return false; // 读不到 build → 无法归因 → 不猜
    };
    // shipped build：读链已由真机证据覆盖（无需探测）
    if profile_for(&ep.platform, build).is_some() {
        return true;
    }
    // 未知 build：探测账本里该 build 的 `state` 行 = probed/verified
    // （键形如 `platform|build|cli_id|epoch`；平台段小写归一，与 fingerprint 一致）
    // LIKE 通配符（`%`/`_`）在 build 段转义——版本串本不该含它们，但转义后
    // 「只认本 build」这条判据不受任何意外字符影响。
    let pattern = format!(
        "{}|{}|%",
        ep.platform.trim().to_ascii_lowercase(),
        build
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    store.with(|conn| {
        conn.query_row(
            "SELECT 1 FROM codex_ipc_probe
              WHERE capability = 'state' AND state IN ('probed', 'verified')
                AND probe_key LIKE ?1 ESCAPE '\\'",
            [pattern],
            |_| Ok(()),
        )
        .is_ok()
    })
}

impl CodexControlHub {
    /// T05：终止类动作（cancel／interrupt）的**共享前置暂停**。
    ///
    /// 语义（spec §7.1／§10）：先持久落行，再发终止指令；暂停建立失败 → 调用方
    /// 必须放弃本次终止（零帧）。返回 `(pause_token, inserted)`：`inserted=false`
    /// 表示同线程已有暂停（幂等，不重复发布）。P3：`pause_turn_id` = 本次停止
    /// 等待的目标回合（expectedTurnId 安全阀）——settle 路径按其终态证据自动清除
    /// 停止类暂停行；None = 未绑定（旧行为：仅显式 resume 可清）。
    /// H01：捕获**原动作**的效果目标（派发前；含 probe key 与包内容 SHA）。
    pub fn capture_observed_target(
        &self,
    ) -> Option<crate::services::codex_desktop_ipc::capability::ObservedTarget> {
        self.adapter.observed_target()
    }

    /// H01：按动作行上**捕获的**目标结算一次成功证据（不读当前目标）。
    pub fn settle_confirmed_effect(&self, action_id: &str) {
        self.router.store().settle_confirmed_effect(action_id);
    }

    pub fn prepare_turn_stop_pause(
        &self,
        key: &ThreadKey,
        reason: &str,
        pause_turn_id: Option<&str>,
    ) -> Result<(String, bool), crate::services::codex_control::StoreError> {
        let pause_token = crate::services::codex_desktop_ipc::types::new_uuid();
        let inserted =
            self.router
                .store()
                .set_queue_pause(key, reason, &pause_token, pause_turn_id)?;
        Ok((pause_token, inserted))
    }

    /// 测试/关闭态装配（计划 Task 8 接口）：一切能力关闭、恒 CompatOnly/拒绝。
    /// adapter 用 closed profile（能力全 false，投影 `compat_only`、写入恒拒）；router
    /// 挂在**内存库**上（fixture 不接触真实 ~/.tuvis）配空派发器（零写入）。
    pub fn disabled() -> Self {
        Self::disabled_with_store(DeviceStore::memory())
    }

    /// [`disabled`](Self::disabled) 的注入存储变体（R8 起普通消息经 router——设备
    /// 复核须与 RemoteState 同库，测试夹具才能走通「派发→NotSent 终态拒发」全臂）。
    #[doc(hidden)]
    pub fn disabled_with_store(store: DeviceStore) -> Self {
        Self::with_parts(
            store,
            Arc::new(DesktopIpcAdapter::new(
                Arc::new(NullConnector),
                closed_profile(),
            )),
            Arc::new(NullDispatch),
            false,
        )
    }

    /// [`disabled`](Self::disabled) 的 Arc 形态（RemoteState 结构字面量测试夹具一行适配用）。
    pub fn disabled_arc() -> Arc<Self> {
        Arc::new(Self::disabled())
    }

    /// 生产装配（remote/mod.rs STATE 消费）：全局 DeviceStore 上的 ActionStore +
    /// AdapterDispatch + 生产 adapter。endpoint 解析 / Desktop build 门按本机事实在
    /// 装配时判定一次：未识别 build → closed profile → A 写能力关闭（不能伪装成已
    /// 验证版本；Desktop 后续才启动的场景保守按不可用，Tuvis 重启后重新判定）。
    pub fn production(store: DeviceStore) -> Self {
        let resolver = production_resolver();
        // #172：profile 从「准入白名单」降级为「证据缓存」——resolve 到 build 但不在
        // shipped 表时用「尚无证据」档（可惰性探测），resolve 失败才是「明确不可用」。
        let profile = match resolver.resolve() {
            Ok(ep) => {
                let platform = ep.platform.clone();
                let build = ep.build.clone().unwrap_or_default();
                profile_for(&platform, &build)
                    .unwrap_or_else(|| ProtocolProfile::unverified(&platform, &build))
            }
            Err(_) => closed_profile(),
        };
        let adapter = Arc::new(DesktopIpcAdapter::new(
            Arc::new(ProductionConnector { resolver }),
            profile,
        ));
        // #172：能力探测账本（`codex_ipc_probe`）——程序自用的证据缓存，与 hub 的
        // ActionStore 同库（设备/动作/探测三类事实一处）。DeviceStore 只在内存库里
        // 需要共享（生产 Global 是零成本的单位变体）。
        adapter.set_probe_ledger(Arc::new(StoreProbeLedger {
            store: store.clone(),
        }));
        // R17/R18：逐能力证据与题提示**接生产存储**（同一 hub 库）：契约识别落账、
        // 重启经当前包核对后复用、真实成功来自动作账本、题事实写穿提示表。
        adapter.set_contract_evidence_store(Some(Arc::new(
            super::codex_evidence::StoreContractEvidence::new(store.clone()),
        )));
        adapter.set_observed_operations_source(Some(Arc::new(
            super::codex_evidence::TargetObservedOperations::new(store.clone()),
        )));
        adapter.set_interaction_hint_store(Some(Arc::new(
            super::codex_evidence::StoreInteractionHints::new(store.clone()),
        )));
        // S1：自动适配门（活值）注入 adapter——新设置 `remote.codex_auto_adapt`
        // 缺省开，显式选择被尊重；adapter 的自动探测门/审批档门/装档门都经它读真值。
        let gate_store = store.clone();
        adapter.set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(super::auto_adapt_enabled_conn)
        })));
        // #172/T7：官方 schema 证据（未知 build 的审批决策词来源）。**一次性**在装配期
        // 后台跑（3s 上限，失败静默）：它只决定「未知 build 的审批卡能不能给
        // accept/cancel」——拿不到证据就保持「到桌面处理」，绝不影响读/发。
        // 只在未知 build 上跑（shipped build 有真机审批证据，不需要）。
        if adapter.probe_eligible() {
            // 同步上下文里 `tokio::spawn` 会 panic（无 reactor）：只在有 runtime 时后台跑
            // （生产装配必在 runtime 内；纯同步测试构造 hub 时安静跳过）。
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let adapter_for_schema = adapter.clone();
                handle.spawn(async move {
                    // #172/T7-R4：取证**与落账本**（`capability = "schema"`）都在
                    // adapter 里——生产层只负责「什么时候探一次」。
                    adapter_for_schema.ensure_schema_evidence().await;
                });
            }
        }
        // FollowGate 单实例同源（R7 复审 P2-1）：AdapterDispatch（ensure 失败回滚判定）
        // 与 hub（SSE 引用计数）必须看同一份关注引用——双实例会让派发侧误判零引用，
        // 把有活跃订阅的 thread 挤出 following 且恢复循环不再重建
        let gate: Arc<FollowGate> = Arc::default();
        let dispatch = Arc::new(AdapterDispatch {
            adapter: adapter.clone(),
            gate: gate.clone(),
            store: store.clone(),
        });
        let mut hub = Self::with_parts(store, adapter, dispatch, true);
        hub.gate = gate;
        // #172/T7：审批回执的消失确认窗口（5s）——受理后等 pending 真的从快照消失
        hub.set_decision_confirm_ms(DECISION_CONFIRM_MS);
        hub
    }

    /// 测试假体构造（计划「测试假体构造」）：注入内存库/adapter/派发器，`capable`
    /// 显式给定——HTTP 集成测试用真 adapter + ScriptedPeer 假 IPC（capable=true），
    /// 「A 不可用」用例用 [`disabled`](Self::disabled)。
    #[doc(hidden)]
    pub fn with_parts(
        store: DeviceStore,
        adapter: Arc<DesktopIpcAdapter>,
        dispatch: Arc<dyn DesktopDispatch>,
        capable: bool,
    ) -> Self {
        let guard_store = store.clone();
        let store = Arc::new(crate::services::codex_control::ActionStore::new(store));
        let router = Arc::new(CodexControlRouter::new(store, dispatch));
        // 终审 F4：**写边界守卫**（mutation 进 writer 队列前的最后判定）——同一
        // 持久撤销标记（人工继续事务写入）+ 兼容许可活值。撤销读取失败 → 保守
        // 拒绝（保守面 = manual_resume_revoked 的 Unknown——与撤销胜出同臂，宁可
        // 定格不可放行首写；DB 修复后用户可显式重试/核对）。
        // 终审 R2：写右判定（连接层注入）——guard Allow 之后帧入队/排队期间的撤销
        // 与活门收回在此原子复核（与人工继续撤销事务同一持久源）。读失败 → 保守拒绝。
        {
            let right_store = guard_store.clone();
            adapter.set_write_right_check_source(Some(Arc::new(
                move |action_id: &str, compat_licensed: bool| {
                    use crate::services::codex_control::WriteGuardVerdict as V;
                    enum Read {
                        Ok { revoked: bool, auto_adapt: bool },
                        Failed,
                    }
                    let read = right_store.with(|c| {
                        let revoked = crate::database::dao::codex_actions::has_write_revoke_conn(
                            c, action_id,
                        );
                        match revoked {
                            Ok(r) => Read::Ok {
                                revoked: r,
                                auto_adapt: super::auto_adapt_enabled_conn(c),
                            },
                            Err(_) => Read::Failed,
                        }
                    });
                    match read {
                        Read::Ok { revoked: true, .. } => V::RejectRevoked,
                        Read::Failed => {
                            // Gate E #5：读失败 ≠ 撤销发生——独立定格码归位
                            //（与 Gate C #4 同纪律；行为仍保守拒）
                            log::error!(
                                "写右判定撤销标记读取失败（保守拒绝首写，独立定格码 write_guard_read_failed）action={action_id}"
                            );
                            V::RejectGuardReadFailed
                        }
                        Read::Ok {
                            revoked: false,
                            auto_adapt,
                        } => {
                            if compat_licensed && !auto_adapt {
                                V::RejectCompatRetracted
                            } else {
                                V::Allow
                            }
                        }
                    }
                },
            )));
        }
        // H02：interrupt 写边界许可的设备有效性只读源（生产与测试装配同源）。
        let device_store = guard_store.clone();
        adapter.set_device_valid_source(Some(Arc::new(move |device_id: &str| {
            device_store.with(|c| {
                crate::remote::pairing::device_valid(
                    c,
                    device_id,
                    chrono::Utc::now().timestamp_millis(),
                )
            })
        })));
        // 复审 R-T1：**取得首次写出权**的 claim（连接层注入，紧随最终判定之后、
        // write_all 之前）——单条两态 CAS（`codex_write_revoke.claimed_at`）与人工
        // 继续撤销事务竞争同一持久行。只读判定（上方）释放 DB 锁后到真正写出之间
        // 的竞争窗（跨 worker 抢占/另一 worker 提交人工继续）由此闭合：撤销先提交
        // → claim 0 行 → 零首次写出；claim 先成功 → 撤销必输，人工继续按设计
        // §6.2「已越过不可撤回写入点只收尾取证」不建后继。兼容许可活值在**同一把
        // DB 锁内**重读（关闭翻转到判定后-claim 前的窗口同样在此拦下）；claim 写
        // 失败 → 保守拒绝首写（宁可不写，不可在未知撤销状态下首写）。
        {
            let claim_store = guard_store.clone();
            let claim_adapter = adapter.clone();
            adapter.set_write_right_claim_source(Some(Arc::new(
                move |action_id: &str, compat_licensed: bool| {
                    use crate::services::codex_control::WriteRightClaimOutcome as C;
                    enum R {
                        Claimed { auto_adapt: bool },
                        Lost,
                    }
                    #[derive(Debug, Clone, Copy)]
                    enum ClaimDenied {
                        DeviceRevoked,
                        ReadFailed,
                    }
                    // R01：先取动作行（kind/thread/request/token）与越队授权事实；
                    // 随后在**资格闸**内复核逐操作资格并立即 claim——判定与首次写出
                    // 权在同一临界区，变化先胜出则零写出。
                    let row = claim_store
                        .with(|c| -> Result<Option<_>, crate::services::codex_control::StoreError> {
                            Ok(crate::database::dao::codex_actions::get_row(c, action_id)
                                .ok()
                                .flatten()
                                .map(|row| {
                                    let bypass = c
                                        .query_row(
                                            "SELECT consumed_at FROM codex_bypass_grant \
                                             WHERE action_id = ?1",
                                            [row.action_id.as_str()],
                                            |r| r.get::<_, Option<i64>>(0),
                                        )
                                        .optional()
                                        .ok()
                                        .flatten()
                                        .flatten()
                                        .is_some();
                                    (row, bypass)
                                }))
                        })
                        .ok()
                        .flatten();
                    let Some((row, bypass)) = row else {
                        return C::ReadFailed;
                    };
                    let kind = match row.kind.as_str() {
                        "send" => crate::services::codex_desktop_ipc::types::ActionKind::Send,
                        "command_approval" => {
                            crate::services::codex_desktop_ipc::types::ActionKind::CommandApproval
                        }
                        "file_change_approval" => {
                            crate::services::codex_desktop_ipc::types::ActionKind::FileChangeApproval
                        }
                        "user_input" => crate::services::codex_desktop_ipc::types::ActionKind::UserInput,
                        "async_question" => {
                            crate::services::codex_desktop_ipc::types::ActionKind::AsyncQuestion
                        }
                        _ => return C::ReadFailed,
                    };
                    let guarded = claim_adapter.with_interaction_verdict(
                        action_id,
                        kind,
                        &row.thread_id,
                        row.request_key.as_deref(),
                        row.context_token.as_deref(),
                        bypass,
                        || {
                            // H02：**同一次 DB 访问**完成设备核验 + 首次写权 CAS
                            //（旧实现两次 with 之间撤设备仍可写出）。判定即线性化点。
                            // 单个 `with` = 单个 DB 临界区：读行 → 核设备 → CAS 写权。
                            claim_store
                                .with(|c| -> Result<R, ClaimDenied> {
                                    let Some(row) = crate::database::dao::codex_actions::get_row(
                                        c, action_id,
                                    )
                                    .map_err(|_| ClaimDenied::ReadFailed)?
                                    else {
                                        return Err(ClaimDenied::ReadFailed);
                                    };
                                    if !crate::remote::pairing::device_valid(
                                        c,
                                        &row.device_id,
                                        chrono::Utc::now().timestamp_millis(),
                                    ) {
                                        return Err(ClaimDenied::DeviceRevoked);
                                    }
                                    let auto_adapt = super::auto_adapt_enabled_conn(c);
                                    match crate::database::dao::codex_actions::claim_write_right_conn(
                                        c,
                                        action_id,
                                        crate::services::codex_control::queue::now_ms(),
                                    ) {
                                        Ok(true) => Ok(R::Claimed { auto_adapt }),
                                        Ok(false) => Ok(R::Lost),
                                        Err(_) => Err(ClaimDenied::ReadFailed),
                                    }
                                })
                        },
                    );
                    let r: Result<R, ClaimDenied> = match guarded {
                        Ok(r) => r,
                        Err(reason) => {
                            log::info!(
                                "claim：交互资格复核拒绝（零首次写出 reason={reason}）action={action_id}"
                            );
                            return C::InteractionDenied(reason);
                        }
                    };
                    match r {
                        Ok(R::Claimed { auto_adapt }) => {
                            if compat_licensed && !auto_adapt {
                                log::info!(
                                    "claim：兼容活门已收回（零投递 capability_unverified）action={action_id}"
                                );
                                C::CompatRetracted
                            } else {
                                C::Claimed
                            }
                        }
                        Ok(R::Lost) => C::LostToRevoke,
                        Err(ClaimDenied::DeviceRevoked) => {
                            log::info!(
                                "claim：设备已撤销（零首次写出）action={action_id}"
                            );
                            C::InteractionDenied("device_revoked".into())
                        }
                        Err(ClaimDenied::ReadFailed) => {
                            log::error!(
                                "首次写出权 claim 写入失败（保守拒绝首写，定格码 write_guard_read_failed）action={action_id}"
                            );
                            C::ReadFailed
                        }
                    }
                },
            )));
        }
        // 复审 R-T4：队列暂停事实源（投影盖章 + republish 消费）——读失败按 true
        // 保守（与 router 的 queue_paused_or_fallback 同口径）；「暂停中 = EXISTS
        // 任意行」由 DAO 单点承载。
        {
            let pause_store = guard_store.clone();
            adapter.set_queue_pause_source(Some(std::sync::Arc::new(move |thread_id: &str| {
                use crate::database::dao::codex_actions::queue_pause_conn;
                pause_store
                    .with(|c| queue_pause_conn(c, thread_id))
                    .map(|p| p.is_some())
                    .unwrap_or(true)
            })));
        }
        adapter.set_write_guard(Some(Arc::new(
            move |action_id: &str, compat_attempt: bool| {
                use crate::services::codex_control::WriteGuardVerdict as V;
                // 单闭包一次锁内完成两读（DeviceStore::with 不可重入——禁止嵌套调用）
                enum Read {
                    Ok { revoked: bool, auto_adapt: bool },
                    Failed,
                }
                let read = guard_store.with(|c| {
                    let revoked =
                        crate::database::dao::codex_actions::has_write_revoke_conn(c, action_id);
                    match revoked {
                        Ok(r) => Read::Ok {
                            revoked: r,
                            auto_adapt: super::auto_adapt_enabled_conn(c),
                        },
                        Err(_) => Read::Failed,
                    }
                });
                match read {
                    Read::Ok { revoked: true, .. } => V::RejectRevoked,
                    Read::Failed => {
                        // Gate C #4：读失败 ≠ 撤销发生——独立定格码
                        // write_guard_read_failed，避免排障把 DB 故障误读成人工继续。
                        log::error!(
                            "写边界撤销标记读取失败（保守拒绝首写，独立定格码 write_guard_read_failed）action={action_id}"
                        );
                        V::RejectGuardReadFailed
                    }
                    Read::Ok {
                        revoked: false,
                        auto_adapt,
                    } => {
                        // 兼容许可活值：排队/await 期间开关可能被关闭——写出边界重读
                        if compat_attempt && !auto_adapt {
                            V::RejectCompatRetracted
                        } else {
                            V::Allow
                        }
                    }
                }
            },
        )));
        Self {
            router,
            adapter,
            events: tokio::sync::broadcast::channel(64).0,
            gate: Arc::default(),
            capable,
            running: AtomicBool::new(false),
            sched: Mutex::new(None),
            reconcile_period_ms: AtomicU64::new(RECONCILE_PERIOD_MS),
            decision_confirm_ms: AtomicU64::new(0),
            front_budget_ms: AtomicU64::new(
                crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS,
            ),
            verify_budget_ms: AtomicU64::new(
                crate::services::codex_desktop_ipc::budget::VERIFY_BUDGET_MS,
            ),
            owner_boot: crate::services::codex_desktop_ipc::types::new_uuid(),
        }
    }

    /// #172/T7：审批回执「消失确认」窗口（毫秒）。生产装配显式设 5000。
    #[doc(hidden)]
    pub fn set_decision_confirm_ms(&self, ms: u64) {
        self.decision_confirm_ms
            .store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    /// S4：单次只读核对预算注参（受控时间测试；生产恒 10 秒常量）。
    #[doc(hidden)]
    pub fn set_verify_budget_ms(&self, ms: u64) {
        self.verify_budget_ms
            .store(ms.max(1), std::sync::atomic::Ordering::SeqCst);
    }

    fn verify_budget_ms(&self) -> u64 {
        self.verify_budget_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// N07：本 hub 的 owner 世系基（boot 身份；同进程重建 hub 也视为新拥有者）。
    pub fn owner_boot(&self) -> &str {
        &self.owner_boot
    }

    /// S3：前台编排预算注参（受控时间测试；生产恒 30 秒常量）。
    #[doc(hidden)]
    pub fn set_front_budget_ms(&self, ms: u64) {
        self.front_budget_ms
            .store(ms.max(1), std::sync::atomic::Ordering::SeqCst);
    }

    /// S3：后台主动收尾预算注参（转发 router；自实际派发起算，排队驻留不占）。
    #[doc(hidden)]
    pub fn set_wrap_up_ms(&self, ms: u64) {
        self.router.set_wrap_up_ms(ms);
    }

    fn front_budget_ms(&self) -> u64 {
        self.front_budget_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn decision_confirm_ms(&self) -> u64 {
        self.decision_confirm_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 测试装配变体：注入 adapter 并以生产形态装配派发缝（AdapterDispatch 包住注入的
    /// adapter——HTTP 集成测试经 ScriptedPeer 驱动完整 IPC 语义；R7.3 引用闸同源共享）。
    #[doc(hidden)]
    pub fn with_adapter(
        store: DeviceStore,
        adapter: Arc<DesktopIpcAdapter>,
        capable: bool,
    ) -> Self {
        let gate: Arc<FollowGate> = Arc::default();
        let dispatch = Arc::new(AdapterDispatch {
            adapter: adapter.clone(),
            gate: gate.clone(),
            store: store.clone(),
        });
        let mut hub = Self::with_parts(store, adapter, dispatch, capable);
        hub.gate = gate;
        hub
    }

    /// hub 事件发送端（动作落账后接力 flush 用）。
    pub(crate) fn notify_settled(&self, key: &ThreadKey) {
        let _ = self.events.send(HubEvent::ThreadSettled(key.clone()));
    }

    /// **A 路发送可用判据**（session-send / send-info 的 hub 分派门；S2 起无兜底交接门）：
    /// 生产 hub 且本 build 的 `send` 能力已由证据确认。disabled hub 恒 false；
    /// 未识别 Desktop build 同 false（「未经验证的 Desktop 版本或方法不得盲目启用
    /// 对应能力」）。与 [`state_available`](Self::state_available) **解耦**：写能不能开
    /// 只看 `send`，不因为读链未取证而连坐。
    pub fn send_available(&self) -> bool {
        self.capable && self.adapter.capabilities().send
    }

    /// **A 路只读可用判据**（详情投影 / SSE / 审批待办读取）：只读链是否可用。
    /// 由 `state` 独立决定（#172 解耦）——探测证明 `state` 可用即可看，不必等 `send`
    /// 打开；旧实现 `send && state` 把只读取证结果绑死在写能力上。
    pub fn state_available(&self) -> bool {
        self.capable && self.adapter.capabilities().state
    }

    /// S2 正名（原 `build_send_verified`，语义见 adapter 字段注释）：最近一次成功
    /// 建连档位的 send 能力位快照。**不再表达「已验证」**、也不再是兜底门（兜底臂
    /// 已删）；现消费者 = 「未知 build 可尝试发送」判据与发送结果记账。
    pub fn profile_send_capable(&self) -> bool {
        self.capable && self.adapter.profile_send_capable()
    }

    /// **S2 可发送判据·静态部分（单点）**：session-send 与 send-info 同源消费。
    /// 实时部分（pending/忙态/状态可确认）在 router 两道门（`SendGate`）。
    /// disabled hub 恒不可尝试（`capability_unverified`）。
    pub fn send_gate_static(&self) -> Result<(), &'static str> {
        if !self.capable {
            return Err("capability_unverified");
        }
        self.adapter.send_gate_static()
    }

    /// #172/T8：**兼容尝试放行判据**（remote 层单点；S1 语义原样）。
    ///
    /// = 自动适配开 ∧ 本 build 写能力未取证（快照）∧ 控制面可用。已取证档位恒
    /// false。派发侧（[`AdapterDispatch`]）用**同一**设置 + 同一快照事实，两端口径
    /// 因此同源同码。
    pub fn compat_send_allowed(&self, auto_adapt: bool) -> bool {
        self.capable && super::compat_send_allowed(auto_adapt, self.profile_send_capable())
    }

    /// R7.3：详情订阅引用 +1（SSE thread/events 建流前调用）。
    pub fn follow_ref_acquire(&self, key: &ThreadKey) {
        self.gate.acquire(key);
    }

    /// R7.3：详情订阅结束 -1；归零且**无本地待发**（queued 普通消息 / 在册屏障——
    /// 有待发工作的 thread 永不被引用计数挤出）→ unfollow 释放 following 名额。
    pub async fn maybe_release_follow(&self, key: &ThreadKey) {
        let remaining = self.gate.release(key);
        if remaining > 0 {
            return;
        }
        let has_work = self
            .router
            .store()
            .thread_has_pending_work(key)
            .unwrap_or(true);
        if has_work {
            return;
        }
        self.adapter.unfollow(key).await;
    }

    /// R7.2：周期对账间隔（测试收窄用；仅影响本 hub 的调度循环）。
    #[doc(hidden)]
    pub fn set_reconcile_period_ms(&self, ms: u64) {
        self.reconcile_period_ms
            .store(ms.max(1), std::sync::atomic::Ordering::SeqCst);
    }

    fn reconcile_period(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.reconcile_period_ms
                .load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// 生产生命周期：装配调度循环（首个 tick 即启动恢复 R7.1：枚举 queued/屏障在册
    /// 的 thread → ensure 重新 following → settle 接力；此后每周期对账 R7.2；idle
    /// 投影 / 落账事件同样驱动）。幂等；disabled hub 短路（不订阅/不恢复/不调度——
    /// 「remote 关闭时不再订阅/恢复/调度未派发项」）。
    pub fn start(self: &Arc<Self>, st: Arc<RemoteState>) {
        if !self.capable {
            return;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return; // 运行中：幂等
        }
        // C4：热重启统一入口——解除停机态（stop 后 ensure 拒连），装配前恢复可连
        self.adapter.resume();
        let hub = self.clone();
        // 刻意保留裸 tokio::spawn、与 stop 的 tauri::async_runtime::spawn **不对称**：
        // start 只从服务器任务（runtime 内，server.rs）调，裸 spawn 本就安全；而调度
        // 循环挂在调用方 runtime 上是集成测试（tests/codex_ipc_remote.rs）对循环驱动
        // 时序的既有假设——换 tauri 全局 runtime 后循环变跨 runtime 真并发，71 个
        // 集成用例中 2 个确定性红（2026-10-23 合入轮实证）。若未来 start 出现
        // runtime 外的调用方，须连测试假设一起重审，不能只换 spawn 宏
        let handle = tokio::spawn(async move { scheduler_loop(hub, st).await });
        *self.sched.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// 停止调度循环（remote 关闭/热重启；R7.5：调度器 + 启动恢复/周期对账 + 事件
    /// 订阅一并关闭——它们同住本循环；已派发任务（IPC 在途 RPC、detached 派发任务）
    /// 不取消——留至落账或 unknown（T 自结算单点收尾）。
    /// 2026-10-17 复审 C4：adapter 一并进入停机（恢复循环/重拉/转发不再重启、
    /// ensure 不再新建连接）并关闭连接（对端见 EOF）——stop 后推进退避时间也不
    /// 再有新 connect/follow；重新 start（resume）后仅装配一套任务。
    pub fn stop(&self) {
        if !self.capable {
            return;
        }
        self.adapter.begin_shutdown();
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.sched.lock().unwrap_or_else(|e| e.into_inner()).take() {
            h.abort();
        }
        // 连接关闭是异步收尾（conn 锁）：后台执行，peer 侧看到 EOF。
        // 必须 tauri::async_runtime::spawn：本函数经 stop_server_with 从**同步命令**
        // （remote_toggle(false) / 通道热重启）的内联线程调——无 runtime 上下文，
        // 裸 tokio::spawn 直接 panic「no reactor running」，且 panic 落在 WebView2
        // IPC 回调边界不可 unwind → abort 整个进程（v0.6.1 三次切换开关闪退的根因，
        // WER 0xc0000409/FAST_FAIL=7 + mam.log panic 记录实证）
        let adapter = self.adapter.clone();
        tauri::async_runtime::spawn(async move { adapter.shutdown().await });
    }
}

/// S3：迟到证据兜底归账——排空适配器槽位内已成熟的迟到回执，CAS 写回**原动作**
///（reserved/unknown → 真实终态；已终态跳过）。只动动作账本：不注入当前新卡、
/// 不投投影、不触碰能力/owner/审批证据（设计 §5.3）。无绑定动作的迟到结果只丢弃。
async fn converge_late_results(hub: &CodexControlHub) {
    for late in hub.adapter.take_ready_late_results().await {
        // 兜底对账无核对上下文：预算给满核对窗口形状（每条各自 10s 上界；调度
        // 循环节奏下不构成核对路径的软化面——核对已改走定向消费，见 verify ②）
        converge_one_late_result(
            hub,
            late,
            crate::services::codex_desktop_ipc::budget::Deadline::after_ms(hub.verify_budget_ms()),
        )
        .await;
    }
}

/// 单条迟到证据的归账（对账兜底与只读核对步骤②**同一单点**）：Accepted 过身份
/// 校验门（[`validate_late_accepted_outcome`]，回显等待上界 = 传入预算的剩余）。
async fn converge_one_late_result(
    hub: &CodexControlHub,
    late: crate::services::codex_desktop_ipc::late::LateResult,
    budget: crate::services::codex_desktop_ipc::budget::Deadline,
) {
    let Some(action_id) = late.action_id.clone() else {
        return;
    };
    let mut outcome = DispatchOutcome {
        delivery: late.outcome.delivery,
        envelope_id: Some(late.envelope_id.clone()),
        turn_id: late.outcome.result.as_ref().and_then(turn_id_of_result),
        reason_code: None,
        revision: None,
    };
    let mut reason_override: Option<String> =
        Some(crate::services::codex_control::LATE_RESULT_REASON.to_string());
    if outcome.delivery == Delivery::Accepted {
        if let Some((thread_id, cmid)) = hub.router.store().action_thread_and_cmid(&action_id) {
            outcome = validate_late_accepted_outcome(
                &hub.adapter,
                &local_key(&thread_id),
                &cmid,
                outcome,
                budget,
                // 终审 F10：槽位冻结的派发前快照（兜底消费与定向消费同源）
                &late.pre_dispatch_turns,
                // P1：槽位冻结的本次 wire 文本（弱证据比对源；start-turn 槽位才有）
                late.wire_text.as_deref(),
            )
            .await;
            if outcome.reason_code.is_some() {
                // 校验降级：保留降级理由（provenance 让位于 operative 事实）
                reason_override = outcome.reason_code.clone();
            }
        }
    }
    let receipt = ActionReceipt {
        action_id,
        delivery: outcome.delivery,
        route: crate::services::codex_desktop_ipc::types::ControlRoute::DesktopIpc,
        turn_id: outcome.turn_id,
        envelope_id: outcome.envelope_id,
        reason_code: reason_override,
    };
    match hub.router.store().finish_late_result(&receipt) {
        Ok(true) => log::info!(
            "codex_ipc: 迟到回执已归账 action={} delivery={:?}",
            receipt.action_id,
            receipt.delivery
        ),
        Ok(false) => {}
        Err(e) => log::error!(
            "codex_ipc: 迟到归账落库失败 action={} err={e}",
            receipt.action_id
        ),
    }
}

/// 调度循环主体：adapter 投影广播 + hub 落账事件 + 周期对账三路输入，逐 Thread 结算。
/// 首个 tick 立即到点 = R7.1 启动恢复（枚举待发 thread → ensure 重新 following →
/// settle 接力 flush；不打开详情、不手工 flush 也能恢复发送）。
/// 派发结果收敛时限：必须**大于后台收尾预算**（S3 wrap_up 120s——在飞的派发任务
/// 合法持有 reserved 直至收尾结束），再留余量。超过即视为等待者与派发任务都已丢、
/// 结果无人认领（reserved → unknown；S3 前该阈值只需盖过 RPC 15s，故为 60s）。
const STUCK_DISPATCH_MS: i64 = 150_000;

async fn scheduler_loop(hub: Arc<CodexControlHub>, _st: Arc<RemoteState>) {
    let mut proj_rx = hub.adapter.subscribe();
    let mut evt_rx = hub.events.subscribe();
    let mut tick = tokio::time::interval(hub.reconcile_period());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            proj = proj_rx.recv() => match proj {
                // 只对非 stale 投影接力（stale = 连接断了，派发只会白试）
                Ok(p) if !p.stale => {
                    let key = local_key(&p.thread_id);
                    settle_thread(&hub, &key).await;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            evt = evt_rx.recv() => match evt {
                Ok(HubEvent::ThreadSettled(key)) => settle_thread(&hub, &key).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                // R9 复审 P2-2：先收敛被取消等待者的残留状态——超龄 reserved →
                // unknown（重启 recover 同语义），内存 Turn 簿记与账本终态对齐
                //（accepted 补绑 turnId / rejected、cancelled 解除限制；**Unknown
                // 一律保留**——2026-10-17 复审 C1.2：对账不是解除屏障的定时器条件）
                let recovered = hub.router.store().mark_stale_reserved_unknown(STUCK_DISPATCH_MS);
                hub.router.converge_with_ledger();
                // S3 迟到证据兜底归账：等待者丢失 / 收尾任务已结束后的迟到回执在
                // 槽位内成熟——此处消费（同一证据只收敛一次由 DAO CAS 保证；先于
                // 本消费被派发任务拿走的槽位在此查无此条）。
                converge_late_results(&hub).await;
                // PR #165 复审 Important：终态行保留清理（墓碑化）随周期对账触发——
                // 快 SQL（partial index 范围 SEEK + LIMIT 分批），不构成新的串行阻塞点；
                // 调度循环本体已是解耦风格，本调用与 mark_stale 同层同性质。
                hub.router.store().tombstone_expired_terminal();
                // R7.1 启动恢复（首 tick 即到点）/ R7.2 周期对账：枚举全部待发 thread
                //（queued 普通消息 / 在册屏障 / 刚收敛的行；空集合零开销），逐线程
                // ensure 重新 following → settle 接力（内部 try-lock 让路）
                let mut keys = hub
                    .router
                    .store()
                    .threads_with_pending_work()
                    .unwrap_or_default();
                for (_, tid) in &recovered {
                    let k = local_key(tid);
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                for key in keys {
                    settle_thread(&hub, &key).await;
                }
            },
        }
    }
}

/// 仅按终态证据结算屏障，不触发后续消息派发；插队持锁期间复用。
async fn settle_turn_evidence(
    hub: &CodexControlHub,
    key: &ThreadKey,
    projection: &ThreadProjection,
) {
    // ① R6.3：投影驱动的顺序限制释放（idle 边沿 / 周期对账都经此处，不只靠手工 flush）
    hub.router.release_turn_if_complete(key, projection);
    // ② fence reconcile（只读，不改 Codex 数据文件；失败/证据不足保持 fence 在册）。
    //    P2（2026-10-30）：fence 分两路核对——
    //    * rollout 证据齐备（compat bridge 路径原子登记）→ 既有 rollout 尾窗核对；
    //    * native 保守屏障（route=desktop_ipc，无 rollout 证据——取不到就不伪造）
    //      但有 wire 文本 hash → **镜像只读核对**：正文 hash 唯一匹配的 userMessage
    //      所在 Turn 以终态在场才放行（纪律不变：仅 idle/到点都不算完成证据）。
    if let Ok(fences) = hub.router.store().compat_fences(key) {
        for fence in fences {
            let verified = match (
                fence.rollout_path.as_deref(),
                fence.rollout_baseline,
                fence.wire_hash.as_deref(),
            ) {
                (Some(_), Some(_), Some(_)) => {
                    matches!(reconcile_compat_fence(&fence, projection).await, Ok(true))
                }
                (_, _, Some(hash)) => {
                    native_unknown_mirror_reconcile(hub, key, &fence, hash, projection).await
                }
                // 证据字段缺失（旧数据无从核对）→ 保持屏障
                _ => false,
            };
            if verified {
                match hub.router.store().clear_compat_fence(&fence.action_id) {
                    Ok(()) => log::info!(
                        "codex_ipc: 兼容 fence 完成核对通过，已解除 action={}",
                        fence.action_id
                    ),
                    Err(e) => {
                        log::warn!(
                            "codex_ipc: fence 清除失败 action={} err={e}",
                            fence.action_id
                        )
                    }
                }
            }
        }
    }
}

/// P2：native（route=desktop_ipc）保守屏障的**镜像只读核对**——
/// * 绑定 turnId 的 unknown：该 Turn 内恰一条正文 hash 匹配的 userMessage，且该
///   Turn 以终态在场（completed/failed/interrupted）→ 完成证据成立；
/// * 未绑 turnId 的 unknown（回执无法关联的保守屏障）：全镜像**唯一**一条正文
///   hash 匹配的 userMessage，其所在 Turn 以终态在场 → 完成证据成立；
/// * 多义（≥2 条同 hash，任何 Turn）/ 无匹配 / stale 投影 → 保持屏障（不猜）。
///
/// 只读核对，不改 Codex 数据文件；窗口截断等镜像不完整形态按「证据不足」保守。
async fn native_unknown_mirror_reconcile(
    hub: &CodexControlHub,
    key: &ThreadKey,
    fence: &CompatFence,
    wire_hash: &str,
    projection: &ThreadProjection,
) -> bool {
    if projection.stale {
        return false;
    }
    if let Some(turn) = fence.turn_id.as_deref() {
        let count = hub
            .adapter
            .count_turn_user_messages_with_text_hash(key, turn, wire_hash)
            .await;
        if count != 1 {
            return false;
        }
        return hub.adapter.turn_evidence_terminal(key, turn);
    }
    let Some(turn) = hub
        .adapter
        .unique_turn_of_user_message_text_hash(key, wire_hash)
        .await
    else {
        return false;
    };
    hub.adapter.turn_evidence_terminal(key, &turn)
}

/// P3（2026-10-30 用户裁决）：停止类暂停的**终态自动解除**。
///
/// 停止/取消端点建立的暂停行（interrupt_pending / interrupt_accepted /
/// interrupt_uncertain / cancel_pending）绑定其等待的目标回合（pause_turn_id =
/// expectedTurnId 安全阀）。当该回合拿到终态证据时，停止已生效、队列不再需要为
/// 「停止在途」冻结——逐行清除（只动该行自己的 token），随后 flush 按 FIFO 自然
/// 接力。证据门与 router 的 `release_turn_if_complete` 同源：投影非 stale +
/// runtime idle/failed + 该 Turn 实体以终态在场（completed/failed/interrupted）；
/// **回合未终态 = 停止未生效 → 暂停保持**。未绑定回合的暂停行（旧行/取不到阀值
/// 的 cancel）保守保留，仅显式 resume 可清。非停止类暂停行不受影响。
/// 返回是否有行被清除（调用方据此重发布投影）。
async fn auto_release_stop_pauses(
    hub: &CodexControlHub,
    key: &ThreadKey,
    projection: &ThreadProjection,
) -> bool {
    if projection.stale {
        return false;
    }
    if !matches!(projection.runtime_status.as_str(), "idle" | "failed") {
        return false;
    }
    let Ok(rows) = hub.router.store().stop_pause_rows(key) else {
        return false;
    };
    if rows.is_empty() {
        return false;
    }
    let mut cleared = false;
    for (token, turn_id) in rows {
        let Some(token) = token.as_deref() else {
            continue; // 旧行无 token：无法定向清除，保守保留
        };
        let Some(turn) = turn_id.as_deref() else {
            continue; // 未绑定目标回合：证据无从核对，保守保留（仅显式 resume）
        };
        if !hub.adapter.turn_evidence_terminal(key, turn) {
            continue; // 停止未生效/结果仍未知：暂停保持
        }
        match hub.router.store().clear_queue_pause_owned(key, token) {
            Ok(true) => {
                log::info!(
                    "codex_ipc: 停止类暂停随回合终态自动解除 thread={} turn={turn}",
                    key.thread_id
                );
                cleared = true;
            }
            Ok(false) => {}
            Err(e) => log::error!(
                "codex_ipc: 停止类暂停自动解除失败（保守保留）thread={} err={e}",
                key.thread_id
            ),
        }
    }
    if cleared {
        // 暂停状态翻转 → 重发布投影（queuePaused 翻 false；发布侧按值去重）
        hub.adapter.republish_thread_projection(key).await;
    }
    cleared
}

/// 单 Thread 结算：① R6.3 Turn 终态释放（新鲜 idle/failed 投影 + 绑定 turnId +
/// revision 严格后于基线 → 清持久屏障与内存限制）；② fence 只读核对（有对应 Turn
/// 完成证据才 clear——Task 7 reconcile 纪律：仅 idle/「已消费」都不算完成）；
/// ③ 接力 flush（S2：派发结果——含 NotSent 终态——都在 router 内落账，无兜底交接；
/// 暂停中的旧队列由 flush 自身冻结，不经此处放行）。
/// 2026-10-17 复审 C2：有待发工作的 Thread 投影源为空/stale（完整重启后无镜像、
/// 同 socket 失效后视图过期）时**主动 ensure 重建**（connect→discovery→following→
/// snapshot，有界 10s）——不能只读空镜像等用户打开详情；释放/接力/屏障核对都
/// 依赖这份新鲜投影。
#[doc(hidden)]
pub async fn settle_thread_for_tests(hub: &Arc<CodexControlHub>, key: &ThreadKey) {
    settle_thread(hub, key).await;
}

async fn settle_thread(hub: &Arc<CodexControlHub>, key: &ThreadKey) {
    let mut projection = hub.adapter.projection(key).await;
    if (projection.stale || projection.revision.is_none())
        && hub
            .router
            .store()
            .thread_has_pending_work(key)
            .unwrap_or(false)
    {
        if let Ok(Ok(p)) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            hub.adapter.ensure_thread(key),
        )
        .await
        {
            projection = p;
            // Desktop 不在/超时：保持 stale 投影照常结算（派发只会白试，闸照常）
        }
    }
    let projection = projection;
    settle_turn_evidence(hub, key, &projection).await;
    // ②′ P3（2026-10-30 用户裁决）：停止类暂停的终态自动解除——停止/取消建立的
    // 暂停只在「停止在途 / 结果未知」期间作为派发闸存在；其等待的回合拿到终态
    // 证据（沿用 release_turn_if_complete 的证据门：投影非 stale + 绑定 turnId +
    // 该 Turn 实体终态在场 + runtime idle/failed）即自动清除该线程的停止类暂停行，
    // 队列按 FIFO 自然接力（下方 flush）。回合未终态（打断未生效/仍在跑）→ 暂停
    // 保持不动（「停止未生效就不要放行」的保护保留）；非停止类暂停行不受影响。
    auto_release_stop_pauses(hub, key, &projection).await;
    // ③ 接力下一条（try-lock 让路：在飞派发占锁时本函数直接 no-op）。
    // S2：UseFallback 消费臂随兜底臂删除——派发结果（含 NotSent 终态）都在
    // router 内落账，调用方无需再做任何「交接」消费。
    if let Some(outcome) = hub.router.flush_thread(key).await {
        log::debug!("codex_ipc: flush 完成（{outcome:?}）");
    }
    // 复审 R-T4：settle 路径的暂停态重发布（事件驱动、零轮询）——线程处于暂停
    // 中时，settle（动作落账/回合终态）驱动的重发布保证 `queuePaused` 最终一致
    //（覆盖前置落行时点镜像尚不在场等错过窗口）。非暂停线程零额外帧。
    // 复审 round 4（V1）：重发布按**实际状态变化**收敛——`republish_thread_
    // projection` 内部对 queuePaused 去重（与上次发布值相同即抑制），暂停事实
    // 不变时 settle / 调度循环消费自身通知 / 周期对账都不再产生新帧（旧实现
    // 无外部输入持续自激循环）；真实翻转（落行/撤回/resume）照常发布。
    let paused_now = hub
        .router
        .store()
        .queue_pause(key)
        .map(|p| p.is_some())
        .unwrap_or(false);
    if paused_now {
        hub.adapter.republish_thread_projection(key).await;
    }
}

// ============================================================
// 派发缝实现（A 路 = DesktopIpcAdapter；disabled = 空派发器）
// ============================================================

/// RpcOutcome → DispatchOutcome（R6.3a：turnId 按**真实封套**解包——`result.turn.id`
/// 之外，Mac Probe 记录的是嵌套两层 `result.result.turn`；缺 turnId 如实回 None，
/// 不视为可完成）。`revision` 由调用方附（回执时投影基线）。
fn outcome_of(out: crate::services::codex_desktop_ipc::connection::RpcOutcome) -> DispatchOutcome {
    let turn_id = out.result.as_ref().and_then(turn_id_of_result);
    DispatchOutcome {
        delivery: out.delivery,
        envelope_id: Some(out.envelope_id),
        turn_id,
        reason_code: out.reason_code,
        revision: None,
    }
}

/// start-turn 回执的 turnId 解包：`result.turn.id`（两层）优先，嵌套两层
/// `result.result.turn.id`（Mac Probe 实录形态）兜底；都不是 → None。
fn turn_id_of_result(result: &serde_json::Value) -> Option<String> {
    if let Some(id) = result
        .get("turn")
        .and_then(|t| t.get("id"))
        .and_then(|i| i.as_str())
    {
        return Some(id.to_string());
    }
    result
        .get("result")
        .and_then(|t| t.get("turn"))
        .and_then(|t| t.get("id"))
        .and_then(|i| i.as_str())
        .map(str::to_string)
}

fn not_sent_outcome(reason: &str) -> DispatchOutcome {
    DispatchOutcome {
        delivery: Delivery::NotSent,
        envelope_id: Some(crate::services::codex_desktop_ipc::types::new_uuid()),
        turn_id: None,
        reason_code: Some(reason.to_string()),
        revision: None,
    }
}

/// R7.3 关注引用闸：详情订阅（SSE thread/events）持有限额内的引用； AdapterDispatch
/// 与 hub 共享同一份（ensure 失败的名额回滚只在零引用时执行——有订阅者的 thread 由
/// adapter 恢复循环重建跟随，不能被回滚挤出）。
#[derive(Default)]
pub(crate) struct FollowGate {
    refs: Mutex<std::collections::HashMap<ThreadKey, usize>>,
}

impl FollowGate {
    fn acquire(&self, key: &ThreadKey) -> usize {
        let mut refs = self.refs.lock().unwrap_or_else(|e| e.into_inner());
        let n = refs.entry(key.clone()).or_insert(0);
        *n += 1;
        *n
    }

    fn release(&self, key: &ThreadKey) -> usize {
        let mut refs = self.refs.lock().unwrap_or_else(|e| e.into_inner());
        let n = refs.entry(key.clone()).or_insert(0);
        *n = n.saturating_sub(1);
        let remaining = *n;
        if remaining == 0 {
            refs.remove(key);
        }
        remaining
    }

    fn is_held(&self, key: &ThreadKey) -> bool {
        self.refs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
    }
}

/// 回执 Turn 关联判定（纯函数，2026-10-20 复审 P1）：`None` = 可以接受为本动作的
/// Turn；`Some(reason)` = 必须按 Unknown 保守处理（不落 Accepted、不绑定完成证据）。
/// - `stale_turn_echo`：回执 turnId 是**派发前已存在**的旧 Turn（Windows 忙态证据）；
/// - `turn_unconfirmed`：success 但缺 turnId——无法确认关联。
pub(crate) fn turn_association_refusal(
    existing_turns: &std::collections::HashSet<String>,
    turn_id: Option<&str>,
) -> Option<&'static str> {
    match turn_id {
        Some(tid) if existing_turns.contains(tid) => Some("stale_turn_echo"),
        Some(_) => None,
        None => Some("turn_unconfirmed"),
    }
}

/// 【Gate B #4 + 终审 F10】迟到 Accepted 回执的**身份校验**（与正常路径 start-turn
/// 同一验证门，迟到回执不降级验证标准）：
/// * Accepted 缺 turnId → `turn_unconfirmed` 保守 Unknown（同 [`turn_association_refusal`]）；
/// * turnId 在场：先查正式消息身份（`userMessage.clientId == 本次 cmid`）——在场
///   即本动作新 Turn（最强证据，Accepted 成立）；不在场而 turnId ∈ **派发前快照**
///   （`pre_dispatch_turns`——预留时随槽位冻结，终审 F10：不得比对当前镜像，新 Turn
///   先广播/正式 userMessage 延后时当前镜像已含本动作 Turn）= **派发前已存在的旧
///   Turn**（忙态「success + 旧 Turn ID」回显形态）→ `stale_turn_echo` 保守 Unknown；
///   两者皆否 → 有界等待正式回显（窗口同正常路径 2s、受剩余预算约束），等不到 →
///   `client_message_unverified` 保守 Unknown。**快照缺失**（空集——审批路径无
///   写前快照点等）不构成 stale 证据：退化为纯身份等待，绝不凭当前镜像判旧。
///
/// 全部降级为 UnknownDelivery（可能已写出，绝不 NotSent）——旧 Turn 常已终态，
/// 不校验会把假 success 记成 Accepted + 绑旧 Turn 并提前放行运行限制。
///
/// P1 弱证据回退（2026-10-30）：`wire_text` = 本次写出的已组装文本（迟到槽位随
/// 四元组冻结）。clientId 强证据仍第一优先；窗口内该 Turn 出现**唯一**一条正文
/// hash 与 wire 一致的 userMessage 时同样判 Accepted（只在已 ack 的同一 turnId
/// 内匹配、多义不采纳、窗口不延长）。
#[allow(clippy::too_many_arguments)]
async fn validate_late_accepted_outcome(
    adapter: &DesktopIpcAdapter,
    key: &ThreadKey,
    client_message_id: &str,
    mut out: DispatchOutcome,
    deadline: crate::services::codex_desktop_ipc::budget::Deadline,
    pre_dispatch_turns: &std::collections::HashSet<String>,
    wire_text: Option<&str>,
) -> DispatchOutcome {
    if out.delivery != Delivery::Accepted {
        return out;
    }
    let Some(turn) = out.turn_id.clone() else {
        out.delivery = Delivery::UnknownDelivery;
        out.turn_id = None;
        out.reason_code = Some("turn_unconfirmed".to_string());
        return out;
    };
    // 正式消息身份已落地 = 本动作新 Turn（强证据，直接成立）
    if adapter
        .turn_has_client_message(key, &turn, client_message_id)
        .await
    {
        return out;
    }
    // Turn 已在**派发前快照**（预留时冻结）= 派发前已存在的旧 Turn（忙态回显）。
    // 终审 F10：不得比对当前镜像——新 Turn 先广播、正式 userMessage 延后（P1 实测
    // +0.5s 常态）时当前镜像已含本动作 Turn，误比对会跳过身份等待、清 turnId，
    // 且槽已消费无法收敛。快照缺失（空集）不构成 stale 证据。
    if pre_dispatch_turns.contains(&turn) {
        log::warn!(
            "codex/thread send: 迟到回执命中派发前已存在旧 Turn（turn={turn}）——按未知投递保守处理"
        );
        out.delivery = Delivery::UnknownDelivery;
        out.turn_id = None;
        out.reason_code = Some("stale_turn_echo".to_string());
        return out;
    }
    // 新 Turn 但正式回显未到：有界等待（窗口同正常路径 2s、受剩余预算约束）
    let weak_hash = wire_text.map(crate::services::codex_control::queue::wire_text_hash);
    let verify_cap = deadline
        .remaining()
        .map(|r| r.min(std::time::Duration::from_secs(2)))
        .unwrap_or(std::time::Duration::from_secs(2));
    let verify_until = tokio::time::Instant::now() + verify_cap;
    loop {
        if adapter
            .turn_has_client_message(key, &turn, client_message_id)
            .await
        {
            return out;
        }
        if let Some(hash) = weak_hash.as_deref() {
            if adapter
                .count_turn_user_messages_with_text_hash(key, &turn, hash)
                .await
                == 1
            {
                log::info!(
                    "codex/thread send: 迟到回执 clientId 缺席但 Turn 内唯一正文 hash 匹配（turn={turn}）——弱证据判 Accepted"
                );
                return out;
            }
        }
        if tokio::time::Instant::now() >= verify_until {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    log::warn!(
        "codex/thread send: 迟到回执 Accepted 但未观测到本次消息身份（turn={turn}）——按未知投递保守处理"
    );
    out.delivery = Delivery::UnknownDelivery;
    out.turn_id = None;
    out.reason_code = Some("client_message_unverified".to_string());
    out
}

/// 生产 A 路派发：普通消息 ensure_thread 先行（owner/following/snapshot 就位——
/// start_turn 需要现行 lease），失败 = 写前零投递 `NotSent`（S2 起 NotSent 即终态回执、
/// 无兜底交接；「不确定已送达绝不自动切换」的边界在 writer 尝试之前，语义安全）。
#[derive(Clone)]
struct AdapterDispatch {
    adapter: Arc<DesktopIpcAdapter>,
    /// R7.3：与 hub 共享的关注引用闸（ensure 失败回滚名额的判断依据）
    gate: Arc<FollowGate>,
    /// #172/T8：设置库（读「兼容发送」开关的**唯一**来源；与 hub 的 ActionStore
    /// 同库同连接——测试夹具写内存库即可切开关）。
    store: DeviceStore,
}

impl AdapterDispatch {
    /// S2 发送守卫新鲜判定（R6.2 忙态首 snapshot + R7.3 失败回滚的超集）：ensure 取
    /// 新鲜投影后分类（StateUnknown > NativePending > Busy > Clear）；ensure 失败
    /// 且无详情订阅者 → unfollow 回滚 following 名额（discovery 失败不耗尽 64 名额），
    /// 返回 `StateUnknown`（按未确认处理——冻结派发，不猜空闲）。
    async fn ensure_and_gate(&self, key: &ThreadKey) -> SendGate {
        let ensured = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            self.adapter.ensure_thread(key).await
        })
        .await;
        let projection = match ensured {
            Ok(Ok(p)) => p,
            Ok(Err(_)) | Err(_) => {
                if !self.gate.is_held(key) {
                    self.adapter.unfollow(key).await;
                }
                return SendGate::StateUnknown;
            }
        };
        classify_send_gate(&projection)
    }

    /// #172/T8：**兼容尝试放行判据**（派发侧）——自动适配开 ∧ 本 build 写能力未取证。
    ///
    /// 与 remote 层（`session_send_via_hub` / `session_send_info`）**同一判据**：
    /// 都读同一设置 + 同一 `profile_send_capable` 快照。已取证档位恒 false。
    fn compat_send_allowed(&self) -> bool {
        // S1：门源 = 新「自动适配」设置（缺省开），与 remote 层 send 门同源。
        super::compat_send_allowed(
            self.store.with(super::auto_adapt_enabled_conn),
            self.adapter.profile_send_capable(),
        )
    }
}

/// S2 守卫分类（优先级 StateUnknown > NativePending > Busy > Clear）：stale 投影 =
/// 状态不可确认（不能借过期快照放行）；任一 pending 在场 = 普通消息零投递；
/// active/unknown 运行态 = 既有忙态留队。
fn classify_send_gate(p: &ThreadProjection) -> SendGate {
    if p.stale {
        return SendGate::StateUnknown;
    }
    if !p.pending.is_empty() {
        return SendGate::NativePending;
    }
    if matches!(p.runtime_status.as_str(), "active" | "unknown") {
        return SendGate::Busy;
    }
    SendGate::Clear
}

impl DesktopDispatch for AdapterDispatch {
    fn thread_busy(&self, key: &ThreadKey) -> bool {
        // R2.3 忙判定：从镜像投影读（快照加性接口 busy_snapshot）；锁不可得按「忙」保守
        self.adapter.busy_snapshot(key)
    }
    /// S2 同步守卫（flush 接力）：无镜像/锁不可得 → StateUnknown（冻结，不盲派）；
    /// 任一 pending → NativePending；忙 → Busy。
    fn send_gate(&self, key: &ThreadKey) -> SendGate {
        let (determinable, busy, pending) = self.adapter.send_gate_snapshot(key);
        if !determinable {
            return SendGate::StateUnknown;
        }
        if pending {
            return SendGate::NativePending;
        }
        if busy {
            return SendGate::Busy;
        }
        SendGate::Clear
    }
    /// 2026-10-17 复审 C1：绑定 Turn 的终态证据——绑定 turnId 的实体在当前镜像中
    /// 以终态在场才放行顺序限制释放（另一个 Turn 完成/泛 idle 不是证据）。
    fn turn_evidence_terminal(&self, key: &ThreadKey, turn_id: &str) -> bool {
        self.adapter.turn_evidence_terminal(key, turn_id)
    }
    fn send_gate_fresh(&self, key: &ThreadKey) -> crate::services::codex_control::BoxSendGateFut {
        let this = self.clone();
        let key = key.clone();
        Box::pin(async move { this.ensure_and_gate(&key).await })
    }
    fn send_text(
        &self,
        key: &ThreadKey,
        action_id: &str,
        composed_text: &str,
        client_message_id: &str,
        deadline: crate::services::codex_desktop_ipc::budget::Deadline,
    ) -> crate::services::codex_control::BoxDispatchFut {
        let adapter = self.adapter.clone();
        let key = key.clone();
        let composed = composed_text.to_string();
        let cmid = client_message_id.to_string();
        let action_id = action_id.to_string();
        // #172/T8：兼容发送许可（入口取一次**只作路由选择**：compat 变体 vs 普通
        // 变体走同一 start_turn 路径）。许可本身的**活值判定**在写边界守卫重读
        // （终审 F4：排队/await 期间关闭开关 → 写出前拒绝 capability_unverified）。
        let compat = self.compat_send_allowed();
        Box::pin(async move {
            // ensure_thread 有界等待（connect 3s + snapshot 5s 量级；Desktop 不在即快败）。
            // S3：上限取 min(10s, 收尾预算剩余)——子步骤只用剩余预算，不重新获得完整上限。
            let ensure_cap = deadline.cap(std::time::Duration::from_secs(10));
            match tokio::time::timeout(ensure_cap, adapter.ensure_thread(&key)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return not_sent_outcome(e.code()),
                Err(_) => return not_sent_outcome("timeout"),
            }
            // **真正写出前的最后一道忙态复核**：router 的 send_gate_fresh 判定之后，
            // Desktop 可能已转 active / 出现 pending（S2：写前出现 pending 同样冻结
            // 回队）。此刻按
            // ensure 后的最新镜像状态判定；忙 → 明确「写前零投递」原因码，router 会把
            // 动作放回队首（不消耗正文、不 fallback、不换 actionId）。
            let (busy, existing_turns) = adapter.pre_write_snapshot(&key);
            if busy {
                return DispatchOutcome {
                    delivery: Delivery::NotSent,
                    envelope_id: None,
                    turn_id: None,
                    reason_code: Some(
                        crate::services::codex_control::BUSY_BEFORE_WRITE_REASON.to_string(),
                    ),
                    revision: adapter.projection(&key).await.revision,
                };
            }
            // #172/T8：兼容发送走**同一 start_turn 路径**（wire 形态不变），只是把
            // 「写能力未取证」这一道能力门在用户显式开启后放开。
            // S3：RPC 等待上界 = 收尾预算剩余（连接层取 min(缺省 RPC 超时, 截止)）。
            let rpc_deadline = deadline
                .remaining()
                .map(|r| tokio::time::Instant::now() + r);
            // 终审 F10：把**此刻**（写出前）的 Turn 集合快照随槽位冻结——迟到回执
            // 的 stale 判定比对这份快照（消费时的当前镜像已可能含本动作新 Turn，
            // 误比对会把有效回执判成旧 Turn 回显）。
            let outcome = if compat {
                adapter
                    .start_turn_compat_bounded(
                        &key,
                        &action_id,
                        &composed,
                        &cmid,
                        rpc_deadline,
                        existing_turns.clone(),
                    )
                    .await
            } else {
                adapter
                    .start_turn_bounded(
                        &key,
                        &action_id,
                        &composed,
                        &cmid,
                        rpc_deadline,
                        existing_turns.clone(),
                    )
                    .await
            };
            // R6.3：回执时投影 revision 基线（完成证据的严格后续判定基准）
            let revision = adapter.projection(&key).await.revision;
            let mut out = outcome_of(outcome);
            out.revision = revision;
            // **Turn 关联有效性**（2026-10-20 复审 P1）：Windows 证据表明忙态
            // start-turn 可能返回 success + 旧 Turn ID，而本次消息并未落进新 Turn。
            // - 回执 turnId ∈ 派发前已存在集合 = 旧 Turn 回显 → 不 Accepted、不绑定、
            //   进保守 Unknown（屏障保留，禁止自动重发/fallback）；
            // - Accepted 却无 turnId = 无法确认关联 → 同样保守 Unknown。
            if out.delivery == Delivery::Accepted {
                if let Some(reason) =
                    turn_association_refusal(&existing_turns, out.turn_id.as_deref())
                {
                    log::warn!(
                        "codex/thread send: start-turn 回执无法确认为本动作新 Turn（action={action_id} turn={:?} reason={reason}）——按未知投递保守处理",
                        out.turn_id
                    );
                    out.delivery = Delivery::UnknownDelivery;
                    out.reason_code = Some(reason.to_string());
                    out.turn_id = None;
                } else if let Some(turn_id) = out.turn_id.clone() {
                    // **正式消息身份核验**（2026-10-20 复审 P1）：Turn ID 新不足以
                    // 证明本条消息进了该 Turn——本地新建的 Turn 会抢在两次快照之间
                    // 出现，忙态回显它时 ID 同样「新」。必须等该 Turn 里出现
                    // `userMessage.clientId == 本次 cmid` 的正式消息；`params.input`
                    // 是请求回显，不作送达证据。有界等待状态帧（正常路径通常先于
                    // 回执到达；预检实测正式 userMessage 后到 515–544ms，窗口取 2s，
                    // 避免正常发送成批误判 Unknown）；等不到才按 Unknown 保守处理。
                    // S3：确认窗口同样受收尾预算剩余约束（不超出预算边界）。
                    // P1 弱证据回退（2026-10-30）：clientId 强证据仍第一优先；窗口内
                    // 若该 Turn 出现**唯一**一条正文 hash 与本次 wire 一致的
                    // userMessage（桌面侧自有消息 id 的回显形态），同样判 Accepted。
                    // 只在已 ack 的同一 turnId 内匹配、多义不采纳、窗口不延长。
                    let weak_hash =
                        crate::services::codex_control::queue::wire_text_hash(&composed);
                    let verify_cap = deadline
                        .remaining()
                        .map(|r| r.min(std::time::Duration::from_secs(2)))
                        .unwrap_or(std::time::Duration::from_secs(2));
                    let verify_until = tokio::time::Instant::now() + verify_cap;
                    let mut verified = false;
                    loop {
                        if adapter.turn_has_client_message(&key, &turn_id, &cmid).await {
                            verified = true;
                            break;
                        }
                        if adapter
                            .count_turn_user_messages_with_text_hash(&key, &turn_id, &weak_hash)
                            .await
                            == 1
                        {
                            log::info!(
                                "codex/thread send: clientId 缺席但 Turn 内唯一正文 hash 匹配（action={action_id} turn={turn_id}）——弱证据判 Accepted"
                            );
                            verified = true;
                            break;
                        }
                        if tokio::time::Instant::now() >= verify_until {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                    if !verified {
                        log::warn!(
                            "codex/thread send: start-turn Accepted 但未观测到本次消息身份（action={action_id} turn={turn_id}）——按未知投递保守处理"
                        );
                        out.delivery = Delivery::UnknownDelivery;
                        out.reason_code = Some("client_message_unverified".to_string());
                        out.turn_id = None;
                    }
                }
            }
            out
        })
    }
    /// S3：在收尾预算内轮询迟到槽位（RPC 超时后捕获的迟到回执）；消费即取走
    ///（同一证据只收敛一次——与后台对账兜底消费竞争时先到先得，DAO 侧 CAS 再兜一层）。
    fn wait_late_result(
        &self,
        envelope_id: &str,
        client_message_id: &str,
        deadline: crate::services::codex_desktop_ipc::budget::Deadline,
    ) -> crate::services::codex_control::BoxLateResultFut {
        let registry = self.adapter.late_registry();
        let env = envelope_id.to_string();
        let cmid = client_message_id.to_string();
        let adapter = self.adapter.clone();
        let store = self.store.clone();
        Box::pin(async move {
            loop {
                if let Some(r) = registry.take_result(&env) {
                    // 协议解包留在 A 路缝（router 不依赖 Desktop wire 形态）：
                    // turnId 解包与 start-turn 正常回执同一单源；envelope 保持原值
                    // 以证「迟到证据绑定原尝试」。
                    let mut out = DispatchOutcome {
                        delivery: r.outcome.delivery,
                        envelope_id: Some(r.envelope_id),
                        turn_id: r.outcome.result.as_ref().and_then(turn_id_of_result),
                        // provenance（late_result）由 router 的 T 统一标记（单点；
                        // 身份校验降级理由不被该标记覆盖——T 侧保留非 None reason）
                        reason_code: None,
                        revision: None,
                    };
                    // Gate B #4：Accepted 迟到回执过**身份校验门**（与正常路径同一
                    // 验证标准）——thread/cmid 经账本单点解析（槽位绑定 actionId）。
                    if out.delivery == Delivery::Accepted {
                        if let Some(action_id) = r.action_id.as_deref() {
                            let row = store
                                .with(|c| {
                                    crate::database::dao::codex_actions::action_thread_and_cmid_conn(
                                        c, action_id,
                                    )
                                })
                                .ok()
                                .flatten();
                            if let Some((thread_id, cmid_of_action)) = row {
                                out = validate_late_accepted_outcome(
                                    &adapter,
                                    &local_key(&thread_id),
                                    if cmid_of_action.is_empty() {
                                        &cmid
                                    } else {
                                        &cmid_of_action
                                    },
                                    out,
                                    deadline,
                                    // 终审 F10：预留时冻结的派发前快照（槽随四元组存）
                                    &r.pre_dispatch_turns,
                                    // P1：预留时冻结的本次 wire 文本（弱证据比对源）
                                    r.wire_text.as_deref(),
                                )
                                .await;
                            }
                        }
                    }
                    return Some(out);
                }
                if deadline.is_expired() {
                    return None;
                }
                let wait = deadline
                    .remaining()
                    .map(|r| {
                        r.min(std::time::Duration::from_millis(
                            crate::services::codex_desktop_ipc::budget::LATE_POLL_INTERVAL_MS,
                        ))
                    })
                    .unwrap_or(std::time::Duration::from_millis(
                        crate::services::codex_desktop_ipc::budget::LATE_POLL_INTERVAL_MS,
                    ));
                tokio::time::sleep(wait).await;
            }
        })
    }
    fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: &PendingAction,
        action_id: &str,
    ) -> crate::services::codex_control::BoxDispatchFut {
        let adapter = self.adapter.clone();
        let key = key.clone();
        let rk = request_key.to_string();
        let tk = context_token.to_string();
        let action = action.clone();
        let action_id = action_id.to_string();
        Box::pin(async move {
            // **不 ensure_thread**：contextToken 只来自投影（客户端必先拉过
            // GET /codex/thread）；镜像缺失/过期由 adapter 最后复核如实拒
            //（stale_context → 409 且零写入）
            outcome_of(adapter.respond(&key, &rk, &tk, action, &action_id).await)
        })
    }

    /// T07：异步正式答题（独立动作类别；不进普通消息 FIFO）。
    fn answer_async(
        &self,
        key: &ThreadKey,
        action_id: &str,
        command: &crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand,
        client_message_id: &str,
        bypass_paused_queue: bool,
        deadline: crate::services::codex_desktop_ipc::budget::Deadline,
    ) -> crate::services::codex_control::BoxDispatchFut {
        let adapter = self.adapter.clone();
        let key = key.clone();
        let action_id = action_id.to_string();
        let command = command.clone();
        let cmid = client_message_id.to_string();
        Box::pin(async move {
            outcome_of(
                adapter
                    .answer_async(
                        &key,
                        &action_id,
                        &command,
                        &cmid,
                        bypass_paused_queue,
                        deadline,
                    )
                    .await,
            )
        })
    }

    /// T04：把前台剩余预算一路传到连接等待层（同一绝对截止，不重置）。
    fn respond_bounded(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: &PendingAction,
        action_id: &str,
        deadline: crate::services::codex_desktop_ipc::budget::Deadline,
    ) -> crate::services::codex_control::BoxDispatchFut {
        let adapter = self.adapter.clone();
        let key = key.clone();
        let rk = request_key.to_string();
        let tk = context_token.to_string();
        let action = action.clone();
        let action_id = action_id.to_string();
        Box::pin(async move {
            outcome_of(
                adapter
                    .respond_bounded(&key, &rk, &tk, action, &action_id, deadline)
                    .await,
            )
        })
    }
}

/// 空派发器（disabled hub）：**恒「不忙」**（R8：未知 build/不可用 hub 的普通消息也要
/// 经 router 登记→reserve→NotSent 终态——恒忙会把消息困死队列）、写恒 NotSent
///（零写入）。
struct NullDispatch;

impl DesktopDispatch for NullDispatch {
    fn thread_busy(&self, _key: &ThreadKey) -> bool {
        false
    }
    fn send_text(
        &self,
        _key: &ThreadKey,
        _action_id: &str,
        _composed_text: &str,
        _client_message_id: &str,
        _deadline: crate::services::codex_desktop_ipc::budget::Deadline,
    ) -> crate::services::codex_control::BoxDispatchFut {
        Box::pin(async { not_sent_outcome("hub_disabled") })
    }
    fn respond(
        &self,
        _key: &ThreadKey,
        _request_key: &str,
        _context_token: &str,
        _action: &PendingAction,
        _action_id: &str,
    ) -> crate::services::codex_control::BoxDispatchFut {
        Box::pin(async { not_sent_outcome("hub_disabled") })
    }
}

/// 恒败连接器（disabled hub）：Desktop IPC 不可达，投影 stale、ensure 恒败。
/// 同时实现 EndpointResolver（production() 在非 macos/windows 平台的诚实占位——
/// resolve 恒败 → closed profile）。
struct NullConnector;

impl crate::services::codex_desktop_ipc::transport::EndpointResolver for NullConnector {
    fn resolve(
        &self,
    ) -> Result<crate::services::codex_desktop_ipc::transport::DesktopEndpoint, ProtocolError> {
        Err(ProtocolError::Disconnected(
            "codex desktop ipc unsupported on this platform".into(),
        ))
    }
}

impl Connector for NullConnector {
    fn connect(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        crate::services::codex_desktop_ipc::transport::BoxIo,
                        ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async {
            Err(ProtocolError::Disconnected(
                "codex desktop ipc disabled".into(),
            ))
        })
    }
}

// ============================================================
// #172：探测账本的存储实现（DeviceStore 上的 `codex_ipc_probe`）
// ============================================================

/// 生产账本：与 hub 的 ActionStore 同库（`~/.tuvis/tuvis.db`；测试注入内存库）。
///
/// 只做三件事：读一行、按结论 upsert 一行、退避字段由 DAO 单点算。**不**碰正文/
/// 线程标识；`verified` 永不写（`probe::ProbeOutcome::as_state` 只出三种词）。
pub(crate) struct StoreProbeLedger {
    store: DeviceStore,
}

impl ProbeLedger for StoreProbeLedger {
    fn get(&self, probe_key: &str, capability: &str) -> Option<ProbeLedgerEntry> {
        self.store
            .with(|conn| {
                crate::database::dao::codex_ipc_probe::get_conn(conn, probe_key, capability)
                    .ok()
                    .flatten()
            })
            .map(|row| ProbeLedgerEntry {
                // S1：成功证据与最近失败观测分离——「已取证」由 DAO 单点判
                evidenced: row.has_evidence(),
                evidenced_at: row.evidence_at,
                last_seen_at: Some(row.last_seen_at),
                state: row.state,
                next_retry_at: row.next_retry_at,
                // #172/T9a：探测只读事实要用的两个字段（只读；不改既有语义）
                stage: row.probe_stage,
                error_code: row.error_code,
                // 证据指纹（只对 capability='schema' 有意义）
                fingerprint: row.fingerprint,
            })
    }

    /// S1：持久 schema 决策词证据（`codex_ipc_schema_evidence` 表）——重启后
    /// 审批档零 CLI 重跑恢复的数据源。
    fn load_schema_evidence(&self, probe_key: &str) -> Option<PersistedSchemaEvidence> {
        self.store
            .with(|conn| {
                crate::database::dao::codex_ipc_probe::get_schema_evidence_conn(conn, probe_key)
                    .ok()
                    .flatten()
            })
            .map(|row| PersistedSchemaEvidence {
                fingerprint: row.fingerprint,
                command_decisions: row.command_decisions,
                file_decisions: row.file_decisions,
                evidenced_at: row.evidence_at,
            })
    }

    fn store_schema_evidence(&self, probe_key: &str, evidence: &SchemaEvidence, at_ms: i64) {
        let stored = self.store.with(|conn| {
            crate::database::dao::codex_ipc_probe::upsert_schema_evidence_conn(
                conn,
                probe_key,
                &evidence.fingerprint,
                &evidence.command_decisions,
                &evidence.file_decisions,
                at_ms,
            )
        });
        if let Err(e) = stored {
            // 写失败只失去「跨重启零重跑」，本次内存证据照常生效；如实记日志。
            log::warn!("codex-ipc schema: 决策词证据落盘失败 key={probe_key} err={e}");
        }
    }

    fn record(
        &self,
        probe_key: &str,
        capability: &str,
        outcome: &ProbeOutcome,
        fingerprint: Option<&str>,
    ) {
        let now = chrono::Utc::now().timestamp_millis();
        let stage = Some(outcome.stage().as_str());
        let recorded = self.store.with(|conn| match outcome {
            ProbeOutcome::Probed { .. } => crate::database::dao::codex_ipc_probe::upsert_ok_conn(
                conn,
                probe_key,
                capability,
                stage,
                fingerprint,
                Some(outcome.duration_ms()),
                now,
            ),
            ProbeOutcome::Failed { error_code, .. } => {
                crate::database::dao::codex_ipc_probe::upsert_fail_conn(
                    conn,
                    probe_key,
                    capability,
                    stage,
                    error_code,
                    fingerprint,
                    Some(outcome.duration_ms()),
                    now,
                )
            }
            ProbeOutcome::Inconclusive { error_code, .. } => {
                crate::database::dao::codex_ipc_probe::upsert_inconclusive_conn(
                    conn,
                    probe_key,
                    capability,
                    stage,
                    error_code,
                    fingerprint,
                    Some(outcome.duration_ms()),
                    now,
                )
            }
        });
        if let Err(e) = recorded {
            // 账本写失败不影响本次探测结论（本进程内存 profile 已按结论提升）；
            // 只失去「跨重启复用」，如实记日志。
            log::warn!("codex-ipc probe: 账本落盘失败 key={probe_key} err={e}");
        }
    }
}

/// 能力全关的保守 profile（未识别平台/build 的诚实缺省——写入恒 `capability_unavailable`）。/// #172：`evidence = Closed` —— **不探测**（这是「明确不可用」档，不是「尚无证据」档）。
fn closed_profile() -> crate::services::codex_desktop_ipc::profile::ProtocolProfile {
    crate::services::codex_desktop_ipc::profile::ProtocolProfile {
        platform: std::env::consts::OS.to_string(),
        build: String::new(),
        capabilities: crate::services::codex_desktop_ipc::types::CapabilitySet {
            send: false,
            state: false,
            command: false,
            permissions: false,
            blocking_input: false,
            interrupt: false,
        },
        method_versions: Default::default(),
        evidence: crate::services::codex_desktop_ipc::profile::EvidenceLevel::Closed,
    }
}

// ============================================================
// #172/T8：兼容发送的结果映射（发送结果 → 探测账本 capability = "send"）
// ============================================================

/// 账本 `capability` 列的**发送**能力词（单一出处 = `probe::CAP_SEND`；此处只做
/// 别名导出防两处漂移）。
pub(crate) const CAP_SEND: &str = crate::services::codex_desktop_ipc::probe::CAP_SEND;
/// 兼容发送「结果未知」的账本原因码（超时 / 断链——**结果未知，绝不重发**）。
///
/// #172/T9a：机器码**单一出处**在 `services::codex_desktop_ipc::reason_code`
/// （前端按 `errors.deliveryUnknown` 取文案）；此处只是别名，值一字不改。
pub(crate) const COMPAT_SEND_DELIVERY_UNKNOWN: &str =
    crate::services::codex_desktop_ipc::reason_code::DELIVERY_UNKNOWN;

/// 兼容发送的一次结果（**只在本路径上产生**：未知 build + 开关开）。
#[derive(Debug, Clone, Copy)]
enum CompatSendOutcome {
    /// 受理（对端确认收到这次 start-turn）→ 账本记 **probed**。
    Accepted,
    /// 写前未投递（连接不可达 / 忙态拒绝 / 方法缺失）→ 账本记 **failed**。
    NotSent,
    /// 结果未知（超时 / 断链）→ 账本记 **inconclusive** + reason `delivery_unknown`。
    Unknown,
}

/// 兼容发送结果 → 探测账本（`capability = "send"`）。
///
/// **为什么记这个**：兼容发送是**用户在风险已知的前提下**放开的通道——它产出的证据
/// 只说明「这个 build 的发送确被受理」，**不**因此写进 shipped 表（提升 shipped
/// 表需要用户裁决，不是程序能自封的）。账本三态与 `codex_ipc_probe` 既有语义同口径：
/// * `probed` = 这次发送确被受理（该 build 的写能力**有**了一次实证）；
/// * `failed` = 写前明确未投递（协议错 / 不可达 / 方法缺失——不自动重试）；
/// * `inconclusive` = 结果未知（**绝不据此重发**，也绝不据此判死）。
///
/// `Rejected`（对端明确拒绝这次动作）**不记账**：一次业务拒绝不等于「这个 build 的
/// 写能力不可用」，把它记成 failed 会把 build 误判成协议错。
///
/// 拿不到 build 目标 → **不记账**（宁可缺行，不可错记到别的 build 上）。
async fn record_compat_send_outcome(st: &Arc<RemoteState>, outcome: CompatSendOutcome) {
    let Some(probe_key) = st.codex_control.adapter.current_probe_key().await else {
        log::info!("codex_ipc: 兼容发送无 build 目标，不记账（宁可缺行，不可错记）");
        return;
    };
    let now = chrono::Utc::now().timestamp_millis();
    let recorded = st.store.with(|conn| match outcome {
        CompatSendOutcome::Accepted => crate::database::dao::codex_ipc_probe::upsert_ok_conn(
            conn, &probe_key, CAP_SEND, None,
            // 发送结论没有内容指纹（只有官方 schema 行带）
            None, None, now,
        ),
        CompatSendOutcome::NotSent => crate::database::dao::codex_ipc_probe::upsert_fail_conn(
            conn,
            &probe_key,
            CAP_SEND,
            None,
            COMPAT_SEND_NOT_SENT_REASON,
            None,
            None,
            now,
        ),
        CompatSendOutcome::Unknown => {
            crate::database::dao::codex_ipc_probe::upsert_inconclusive_conn(
                conn,
                &probe_key,
                CAP_SEND,
                None,
                COMPAT_SEND_DELIVERY_UNKNOWN,
                None,
                None,
                now,
            )
        }
    });
    if let Err(e) = recorded {
        // 账本写失败不影响本次发送结论（回执照常给出）；只失去「跨重启复用」。
        log::warn!("codex_ipc: 兼容发送账本落盘失败 key={probe_key} err={e}");
    }
}

/// 写前未投递的账本原因码（**固定词**，不逐字回抄派发原因码——派发原因码是
/// 本次失败的细节，账本要的是「写前没送出去」这一档事实）。
const COMPAT_SEND_NOT_SENT_REASON: &str = "not_sent_before_write";

// ============================================================
// 审计（动作词表 + summary JSON；凭据零泄漏）
// ============================================================

const AUDIT_SEND: &str = "codex_send";
const AUDIT_APPROVE: &str = "codex_approve";
const AUDIT_REJECT: &str = "codex_reject";
const AUDIT_ANSWER: &str = "codex_answer";
/// 打断桌面当前回合（破坏性动作——审计必须留痕）。
const AUDIT_INTERRUPT: &str = "codex_interrupt";
/// Desktop IPC 动作的审计 channel 词（headless 归属行记**实际 headless 通道名**——由封套取）。
const AUDIT_CHANNEL: &str = "codex_desktop_ipc";

/// #172 用户裁决 + S2 收紧（设计 §4.1）：**桌面会话永不自动转 headless**——不可
/// 尝试时「发到 A 或诚实失败」，headless 只服务 headless-owned 会话。
/// `capability_unverified` = 该 build 的写能力未取证 / 自动适配关闭（403）；
/// 与「已取证 build 瞬时断链」的 `capability_unavailable`（503）拆码。
/// `capability_unavailable`（已取证 build 瞬时断链；与 adapter NotSent 同名码——跨层
/// 各自定义是既有拆码约定，值一字不差）。
pub(crate) const DESKTOP_SEND_UNAVAILABLE: &str = "capability_unavailable";
/// 同上门的用户可读文案（手机端只读 `reason` 展示；不暴露协议细节）。
pub(crate) const DESKTOP_SEND_UNVERIFIED_REASON: &str =
    "当前 Codex 桌面版本未验证远程发送能力，已拒绝以免消息落到别的会话；请到电脑端处理";
/// 已取证 build 瞬时断链的用户可读文案（503；消息不发送也不排队——零投递保留草稿）。
pub(crate) const DESKTOP_SEND_UNAVAILABLE_REASON: &str =
    "电脑端 Codex 暂时不可达，本次未发送；请稍后重试";

/// Codex 控制动作审计单点：`summary` = JSON
/// `{actionId,threadId,requestKey?,envelopeId?,result}`。
///
/// **凭据零泄漏红线（spec S6）**：原 permissions、答案原文、cookie、API key 哨兵绝不
/// 进 summary/日志——本函数只收 id 与结果词，结构上没有可泄漏的通道。
fn codex_audit(
    st: &RemoteState,
    ctx: &ActionContext,
    action: &str,
    session_id: &str,
    request_key: Option<&str>,
    envelope_id: Option<&str>,
    result: &str,
) {
    let summary = serde_json::json!({
        "actionId": ctx.action_id,
        "threadId": ctx.key.thread_id,
        "requestKey": request_key,
        "envelopeId": envelope_id,
        "result": result,
    });
    st.store.with(|c| {
        record_conn(
            c,
            chrono::Utc::now().timestamp_millis(),
            &ctx.device_id,
            &ctx.device_name,
            "codex",
            session_id,
            AUDIT_CHANNEL,
            action,
            &summary.to_string(),
            result,
        );
    });
}

/// delivery → 审计结果词（delivery snake_case + 机器原因码）。
fn delivery_word(d: Delivery) -> &'static str {
    match d {
        Delivery::NotSent => "not_sent",
        Delivery::Accepted => "accepted",
        Delivery::Rejected => "rejected",
        Delivery::UnknownDelivery => "unknown_delivery",
    }
}

// ============================================================
// 会话/Thread 解析（session → local ThreadKey 单点）
// ============================================================

/// 会话定位（spawn_blocking 内调用：快照扫描是重活；与 api::find_session_sync 同源）。
pub(crate) fn find_session_sync(
    st: &Arc<RemoteState>,
    session_id: &str,
) -> Option<crate::session::Session> {
    (st.session_source)()
        .sessions
        .into_iter()
        .find(|s| s.id == session_id)
}

/// session → local ThreadKey（服务端 resolve_thread 单点：rollout 文件名 UUID 权威 →
/// 仅 UUID 形态会话号回退；非 codex / 无法解析 → None——HTTP 层 404）。
pub(crate) fn resolve_session_thread(
    st: &RemoteState,
    session: &crate::session::Session,
) -> Option<ThreadKey> {
    if session.agent_type.tool_id() != "codex" {
        return None;
    }
    let home = (st.home_source)().map(std::path::PathBuf::from);
    let rollout = production_rollout_path(home.as_deref(), &session.id);
    resolve_thread(rollout.as_deref(), &session.id).map(|t| local_key(&t.id))
}

// ============================================================
// session-send 的 hub 分派臂（api::session_send 前置调用）
// ============================================================

/// Codex APP 会话经 control hub 发送（api::session_send 在 H3 门之前调用本函数）。
/// **S2**：Desktop 会话永不自动转 headless——本函数没有任何兜底分支。
///
/// 契约（计划 Task 8 端点表 + S2 修订）：
/// - Codex APP **必须带 actionId**（缺失 400——不代造，防重发双投）；
/// - **可发送判据·静态门（与 send-info 同源单点）**：`hub.send_gate_static()` 不可
///   尝试 → 403（`capability_unverified`）/ 503（`capability_unavailable`——已取证
///   build 的瞬时断链），**零账本写入**（从未进入尝试，重放同 ID 同样在门处拒绝）；
/// - `Completed(receipt)` → `{status:"desktop_ipc", receipt}`（delivery 原样，200——
///   unknown_delivery 是明确回执不是错误；NotSent = 明确零投递终态，**无第三去向**）；
/// - `Queued` → `{status:"codex_queued", actionId, position}`；
/// - `bypassPausedQueue` 透传 router（暂停 Thread 的一次性越队授权，见 router）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn session_send_via_hub(
    st: &Arc<RemoteState>,
    session: &crate::session::Session,
    device_id: &str,
    device_name: &str,
    text: &str,
    action_id: &str,
    queue_only: bool,
    bypass_paused_queue: bool,
) -> Response {
    let hub = &st.codex_control;
    let Some(key) = resolve_session_thread(st, session) else {
        // thread 不可解析（rollout 无 UUID 且会话号非 UUID）：A 路无法靶向——按 hub
        // 分支的 404 语义如实拒绝
        return not_found();
    };
    let ctx = ActionContext {
        action_id: action_id.to_string(),
        device_id: device_id.to_string(),
        device_name: device_name.to_string(),
        kind: ActionKind::Send,
        key: key.clone(),
    };
    // S2 静态门（单点）：不可尝试 → 诚实拒绝，不登记不排队（与 send-info 同源同码）。
    if let Err(code) = hub.send_gate_static() {
        let (status, reason) = static_gate_refusal(code);
        codex_audit(
            st,
            &ctx,
            AUDIT_SEND,
            &session.id,
            None,
            None,
            &format!("refused:{code}"),
        );
        return json_no_store(
            status,
            serde_json::json!({ "error": code, "reasonCode": code, "reason": reason }),
        );
    }
    // 兼容尝试记账判据（**与派发侧同一判据**：同一设置 + 同一 profile_send_capable
    // 快照）——S2 修正：此处曾误读旧「兼容发送（实验）」键（S1 拆接线遗漏点）。
    let compat_attempt = hub.compat_send_allowed(st.store.with(super::auto_adapt_enabled_conn));
    // S3：前台编排预算自**服务端接收请求**起算（30 秒含连接/探测/复核/回执/确认；
    // 子步骤共用同一绝对截止，不逐层重置）。
    let front =
        crate::services::codex_desktop_ipc::budget::Deadline::after_ms(hub.front_budget_ms());
    match hub
        .router
        .send_bounded(
            ctx.clone(),
            text.to_string(),
            queue_only,
            bypass_paused_queue,
            front,
        )
        .await
    {
        RouterOutcome::Completed(receipt) => {
            // 兼容尝试的结果映射（未知 build 写尝试的证据采集，设计 §3.3「发送本身
            // 提供真实写证据」）：
            // - Accepted → 账本记 probed（该 build 的发送确被受理；**不**写进 shipped
            //   表——提升 shipped 表需要用户裁决）；
            // - Unknown（超时/断链）→ 账本记 inconclusive（**不重发**）；
            // - NotSent（写前未投递实证：方法缺失/lease 失效等）→ 账本记 failed。
            //   S2 起该记录从「兜底分支」移到Completed 臂——兜底臂已删，NotSent 就是
            //   写前未投递的终态事实。
            if compat_attempt {
                // S3：迟到收敛回执（reason=late_result）只归原动作，**不**写探测账本
                //——旧连接的真实回执不得当成新连接的能力证据（设计 §5.3）。
                let late_receipt = receipt.reason_code.as_deref()
                    == Some(crate::services::codex_control::LATE_RESULT_REASON);
                match (receipt.delivery, late_receipt) {
                    (_, true) => {}
                    (Delivery::Accepted, false) => {
                        record_compat_send_outcome(st, CompatSendOutcome::Accepted).await
                    }
                    (Delivery::UnknownDelivery, false) => {
                        record_compat_send_outcome(st, CompatSendOutcome::Unknown).await
                    }
                    (Delivery::NotSent, false) => {
                        record_compat_send_outcome(st, CompatSendOutcome::NotSent).await
                    }
                    (Delivery::Rejected, false) => {}
                }
            }
            codex_audit(
                st,
                &ctx,
                AUDIT_SEND,
                &session.id,
                None,
                receipt.envelope_id.as_deref(),
                &audit_result_of_receipt(&receipt),
            );
            json_no_store(
                StatusCode::OK,
                serde_json::json!({ "status": "desktop_ipc", "receipt": receipt }),
            )
        }
        RouterOutcome::Queued {
            action_id,
            position,
        } => {
            codex_audit(st, &ctx, AUDIT_SEND, &session.id, None, None, "queued");
            json_no_store(
                StatusCode::OK,
                serde_json::json!({
                    "status": "codex_queued",
                    "actionId": action_id,
                    "position": position,
                }),
            )
        }
    }
}

/// 静态门拒绝 → (HTTP 状态码, 用户可读中文原因)。`capability_unverified` = 未取证/
/// 未开启（403，引导回电脑端处理）；`capability_unavailable` = 已取证 build 的瞬时
/// 断链（503，可稍后重试）。
pub(crate) fn static_gate_refusal(code: &str) -> (StatusCode, &'static str) {
    match code {
        DESKTOP_SEND_UNAVAILABLE => (
            StatusCode::SERVICE_UNAVAILABLE,
            DESKTOP_SEND_UNAVAILABLE_REASON,
        ),
        _ => (StatusCode::FORBIDDEN, DESKTOP_SEND_UNVERIFIED_REASON),
    }
}

/// 回执 → 审计结果词（delivery + 机器原因码，如 `not_sent:stale_context`）。
fn audit_result_of_receipt(r: &ActionReceipt) -> String {
    match r.reason_code.as_deref() {
        Some(code) => format!("{}:{code}", delivery_word(r.delivery)),
        None => delivery_word(r.delivery).to_string(),
    }
}

// ============================================================
// 端点：GET /codex/thread
// ============================================================

/// GET /m/api/v1/codex/thread?session_id=：Session 经服务端 resolve_thread → local
/// ThreadKey → adapter 投影（ensure 优先取活数据，失败/超时回落只读 stale 投影——
/// Desktop 不在是常态而非错误）。
/// 404：会话不在快照 / 非 codex / rollout 无 UUID 且会话号非 UUID。
pub async fn thread_state(
    State(st): State<Arc<RemoteState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let hub = &st.codex_control;
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/thread 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    let projection = live_projection(hub, &key).await;
    // T02：发射点盖章 `projectionVersion`（与 SSE 同源；instance = hub 世系身份）。
    json_no_store(
        StatusCode::OK,
        projection_json_with_version(&projection, hub.owner_boot()),
    )
}

/// 活投影：ensure_thread（有界 10s）成功 → 新鲜投影；失败/超时 → 当前投影（stale）。
/// ensure 的连接副作用建立 owner/following——SSE 与后续写入都依赖它，GET 是首个
/// 关注点注册入口（有界关注集合，不扫全历史）。
/// R7.3：ensure 失败且**无详情订阅者**时 unfollow 回滚 following 名额（64 次失败
/// discovery 不耗尽名额；有订阅者的 thread 交给 adapter 恢复循环重建，不回滚）。
async fn live_projection(hub: &CodexControlHub, key: &ThreadKey) -> ThreadProjection {
    let adapter = hub.adapter.clone();
    let key2 = key.clone();
    match tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        adapter.ensure_thread(&key2).await
    })
    .await
    {
        Ok(Ok(p)) => p,
        _ => {
            if !hub.gate.is_held(key) {
                hub.adapter.unfollow(key).await;
            }
            hub.adapter.projection(key).await
        }
    }
}

// ============================================================
// 端点：GET /codex/thread/messages（同代只读历史分页）
// ============================================================

/// GET /m/api/v1/codex/thread/messages?session_id=&before_id=&history_token=&limit=：
/// 在镜像已取得的 canonical 历史里读取更早消息（计划任务 4）。
///
/// 契约：
/// - `session_id`、`history_token` 必填（缺失/空白 → 400）；`before_id` 可缺省——
///   表示「历史末端锚点」：空历史下与投影 token 一起返回 `end`，非空历史返回
///   `cursor_invalid/cursor_required`（绝不猜成最新页）；
/// - `limit` 缺省 50，允许 1..=50，其余（0/51/非数字/空）→ 400；
/// - 复用远程鉴权（路由位于 gated nest）与 `find_session_sync` +
///   `resolve_session_thread`（错误会话 → 404；跨会话只解析到自己线程）；
/// - 无镜像时复用 GET /codex/thread 的**有界只读** ensure/hydrate（连接 + follow +
///   snapshot + 既有 load-complete-history）；失败不报错，分页按镜像实际状态如实
///   返回 `unavailable`；
/// - 响应 no-store；不触发发送/审批/写入，不重扫 rollout 或磁盘目录，分页在镜像锁内
///   同步完成（不跨 await 持锁）。
pub async fn thread_messages(
    State(st): State<Arc<RemoteState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    // 无历史 token = 拒绝（客户端必须带投影/上一页给出的同代锚点）。
    let Some(history_token) = params
        .get("history_token")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    // limit 缺省 50，允许 1..=50，其余 400（不做静默夹取）。
    let limit = match params.get("limit") {
        None => MESSAGES_PAGE_LIMIT,
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(value) if (1..=MESSAGES_PAGE_LIMIT).contains(&value) => value,
            _ => return bad_request(),
        },
    };
    let before_id = params
        .get("before_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let hub = &st.codex_control;
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(value) => value,
            Err(e) => {
                log::error!("codex/thread/messages 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    // 复用既有只读 hydrate 路径（与 GET /codex/thread 同源）：只在没有镜像时触发，
    // 正常分页不额外连接、不额外投影。失败/超时是常态（Desktop 不在），不影响下面
    // 按既有镜像状态如实分页。
    if !hub.adapter.has_mirror(&key).await {
        let _ = live_projection(hub, &key).await;
    }
    let page = hub
        .adapter
        .messages_page(&key, before_id.as_deref(), &history_token, limit)
        .await;
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&page).unwrap_or_else(|_| {
            serde_json::json!({
                "messages": [],
                "historyToken": "",
                "revision": serde_json::Value::Null,
                "nextBefore": serde_json::Value::Null,
                "status": "unavailable",
                "reasonCode": "serialize_failed",
            })
        }),
    )
}

// ============================================================
// 端点：GET /codex/thread/events（SSE）
// ============================================================

/// GET /m/api/v1/codex/thread/events?session_id=：SSE 首帧 `snapshot`、后续 `state`
/// （均 ThreadProjection）。订阅 adapter 投影广播；64 槽 broadcast lag → 重新 snapshot
/// （下游基线自愈）。复用 SseRegistry：吊销/停止即时断连，自然断开反注册。
pub async fn thread_events(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let hub = &st.codex_control;
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/thread/events 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    // 注册连接（gate 已过闸；防御性 None 跳过注册只服务）
    let device = super::gate::extract_device(&headers).unwrap_or_default();
    let (conn_id, close_rx) = st.sse_registry.register(&device);
    let registry = st.sse_registry.clone();
    let adapter = hub.adapter.clone();
    // R7.3：详情订阅引用计数（挂载 +1；流结束 -1；归零且无本地待发 → unfollow）
    hub.follow_ref_acquire(&key);
    // **先订阅、后取快照**（2026-10-20 复审 P2）：消除「快照 → subscribe」之间唯一
    // 更新永久漏掉的窗口。订阅缓冲里的事件在快照之后按 owner 代际 + revision 合并，
    // 旧事件不得覆盖较新的首快照（见 `merge_startup_buffer`）。
    let mut rx = adapter.subscribe();
    let first = live_projection(hub, &key).await;
    let (first, buffered) = drain_startup_buffer(adapter.clone(), &key, &mut rx, first).await;
    let guarded = GuardedStream {
        inner: projection_stream(
            adapter,
            key.clone(),
            first,
            buffered,
            rx,
            hub.owner_boot().to_string(),
        )
        .take_until(close_rx),
        _guard: Some(FollowReleaseGuard {
            hub: st.codex_control.clone(),
            key,
        }),
    };
    let cleaned = super::server::CleanupStream {
        inner: guarded,
        reg: registry,
        device,
        conn_id,
    };
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Sse::new(cleaned).keep_alive(sse::KeepAlive::default()),
    )
        .into_response()
}

/// R7.3：SSE 详情订阅的关注引用守卫——流结束（自然断开/吊销/服务端关闭同一时机）
/// 引用 -1，归零且无本地待发 → unfollow 释放 following 名额（Drop 不能 await →
/// spawn 后台释放）。
struct FollowReleaseGuard {
    hub: Arc<CodexControlHub>,
    key: ThreadKey,
}

impl Drop for FollowReleaseGuard {
    fn drop(&mut self) {
        let hub = self.hub.clone();
        let key = self.key.clone();
        // Drop 不能 await：后台释放引用（任务自持；进程退出未跑完的残额随进程消失，
        // following 名额由 Desktop 侧连接生命周期兜底）
        tokio::spawn(async move { hub.maybe_release_follow(&key).await });
    }
}

/// 持守卫的流包装：守卫随流一起 drop。
struct GuardedStream<S> {
    inner: S,
    _guard: Option<FollowReleaseGuard>,
}

impl<S: Stream> Stream for GuardedStream<S> {
    type Item = S::Item;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // Safety: 无 Unpin 约束需求经字段投影转发（inner 已被 take_until 包装为 Unpin 流链）
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll_next(cx) }
    }
}

/// 启动缓冲是否比当前快照更新（**不用单一 revision**：跨 owner 会回退）。
/// 顺序：epoch > generation > revision > local_seq（终审 F5 决胜）；stale 事件
/// 不得覆盖新鲜快照。
///
/// **终审 F5**：本地策略更新（schema 证据安装/恢复、探测通过的重发布）不改变
/// Desktop 侧任何标识（revision/代际原样）——同标识时按 `local_seq` 决胜（发布
/// 点单调递增分配），策略帧不再被「同 revision」吞掉。`local_seq` 与 Desktop
/// revision **分离**：不伪造 revision；旧事件的序号必然更小，不可能反超（不
/// 回退）。手工构造的零序号事件（旧测试夹具/非发布路径）与既有语义一致（0 不
/// 大于 0，仍按 revision 竞争）。
fn supersedes(buffered: &ThreadProjection, snap: &ThreadProjection) -> bool {
    if buffered.thread_id != snap.thread_id {
        return false;
    }
    if buffered.owner_epoch != snap.owner_epoch {
        return buffered.owner_epoch > snap.owner_epoch;
    }
    if buffered.owner_generation != snap.owner_generation {
        return buffered.owner_generation > snap.owner_generation;
    }
    if buffered.stale != snap.stale {
        if buffered.stale {
            // stale = 当前视图**失效**（断线/丢帧/invalidate）——必须送达，否则
            // 手机停在旧的可控状态（2026-10-20 复审 P2）。含 revision=None 的
            // invalidate（无旧内容，只把下游标成 stale，保守优先于假可控）；
            // 后续更高 revision 恢复。
            return true;
        }
        // 恢复：只有更高 revision（或快照无 revision）证明世界已更新才允许清除
        // stale；同 revision 的「非 stale」不能证明任何新数据。
        return match (buffered.revision, snap.revision) {
            (Some(b), Some(s)) => b > s,
            (Some(_), None) => true,
            _ => false,
        };
    }
    match (buffered.revision, snap.revision) {
        (Some(b), Some(s)) => b > s || (b == s && buffered.local_seq > snap.local_seq),
        (Some(_), None) => true,
        // 无 revision 的事件（stale/未知）不能证明比快照新；但本地序号仍可证明
        // 是**较新的重发布**（同 revision=None 的空投影原地更新）。
        (None, None) => buffered.local_seq > snap.local_seq,
        (None, Some(_)) => false,
    }
}

/// 首快照 + 启动缓冲合并：快照在前，缓冲里**仍然更新**的事件按到达顺序追加；
/// 与已接受链尾比较（单调），旧事件既不覆盖快照也不回退。
fn merge_startup_buffer(
    key: &ThreadKey,
    first: ThreadProjection,
    buffered: Vec<ThreadProjection>,
) -> Vec<ThreadProjection> {
    let mut out = vec![first];
    for p in buffered {
        if p.thread_id != key.thread_id {
            continue;
        }
        if let Some(last) = out.last() {
            if supersedes(&p, last) {
                out.push(p);
            }
        }
    }
    out
}

/// 取快照后把订阅缓冲抽干并合并；启动期发生 Lagged（丢帧）→ 重新取一次快照并清空
/// 缓冲（有界：最多重取一次，避免高频循环）。
async fn drain_startup_buffer(
    adapter: Arc<DesktopIpcAdapter>,
    key: &ThreadKey,
    rx: &mut tokio::sync::broadcast::Receiver<ThreadProjection>,
    first: ThreadProjection,
) -> (ThreadProjection, Vec<ThreadProjection>) {
    let mut buffered = Vec::new();
    let mut lagged = false;
    loop {
        match rx.try_recv() {
            Ok(p) => {
                if lagged {
                    // Lagged 后仍可能继续有可读事件：抽干但不收进缓冲（无法证明
                    // 它们比快照新）；避免把它们留到流式阶段补发而回退。
                    continue;
                }
                buffered.push(p);
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                lagged = true;
                buffered.clear();
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
    if lagged {
        // 重取快照；取到的是当前投影（若 stale/空则如实交给下游按 stale 处理）
        return (adapter.projection(key).await, Vec::new());
    }
    let merged = merge_startup_buffer(key, first, buffered);
    let mut it = merged.into_iter();
    let head = it.next().expect("merge 至少保留首快照");
    (head, it.collect())
}

/// 投影事件流：首帧 snapshot（含启动缓冲合并出的后续 state），后续 state；Lagged →
/// 重发 snapshot（下游基线自愈）。`instance` = hub 世系身份（发射点盖章
/// `projectionVersion`，与 GET /codex/thread 同源——T02 投影落地守卫）。
#[allow(clippy::type_complexity)]
fn projection_stream(
    adapter: Arc<DesktopIpcAdapter>,
    key: ThreadKey,
    first: ThreadProjection,
    buffered: Vec<ThreadProjection>,
    rx: tokio::sync::broadcast::Receiver<ThreadProjection>,
    instance: String,
) -> impl Stream<Item = Result<sse::Event, Infallible>> {
    let state = StreamState {
        adapter,
        key,
        rx,
        instance,
        last: first.clone(),
        pending_snapshot: Some(first),
        pending_events: buffered.into(),
    };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(first) = s.pending_snapshot.take() {
                s.last = first.clone();
                return Some((Ok(projection_event("snapshot", &first, &s.instance)), s));
            }
            if let Some(p) = s.pending_events.pop_front() {
                s.last = p.clone();
                return Some((Ok(projection_event("state", &p, &s.instance)), s));
            }
            match s.rx.recv().await {
                Ok(p) => {
                    if p.thread_id == s.key.thread_id && supersedes(&p, &s.last) {
                        s.last = p.clone();
                        return Some((Ok(projection_event("state", &p, &s.instance)), s));
                    }
                    // 其他 Thread / 不比已发送状态更新的事件：跳过（单会话订阅；
                    // 旧事件不得在新快照后补发造成回退）
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 丢帧 = 下游可能与真实状态脱节：先把已排队的事件抽干（它们
                    // 无法证明比即将重发的快照新，留着会在快照后补发而回退），
                    // 再重发当前投影 snapshot 校正（计划 Task 8 端点表）。
                    loop {
                        match s.rx.try_recv() {
                            Ok(_) => continue,
                            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                        }
                    }
                    let snap = s.adapter.projection(&s.key).await;
                    s.last = snap.clone();
                    return Some((Ok(projection_event("snapshot", &snap, &s.instance)), s));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

struct StreamState {
    adapter: Arc<DesktopIpcAdapter>,
    key: ThreadKey,
    rx: tokio::sync::broadcast::Receiver<ThreadProjection>,
    /// T02：hub 世系身份（`CodexControlHub::owner_boot`；发射点盖章用）。
    instance: String,
    pending_snapshot: Option<ThreadProjection>,
    /// 最近一次已发送给下游的投影（流式阶段的顺序基线，2026-10-20 复审 P2）
    last: ThreadProjection,
    /// 启动缓冲合并结果（快照之后、订阅转实时之前必须补发的事件）
    pending_events: std::collections::VecDeque<ThreadProjection>,
}

/// T02：投影序列化 + hub 世系版本束盖章（GET /codex/thread 与 SSE snapshot/state
/// **同源**下发 `projectionVersion`；前端据此把 GET／SSE／手动刷新收进同一落地
/// 守卫）。instance 来自调用方持有的 hub（进程级世系，**不**在每次 GET/帧生成）。
/// 序列化失败仍退回既有最小 stale 形状（不给下游可解析的半截投影）。
fn projection_json_with_version(p: &ThreadProjection, instance: &str) -> serde_json::Value {
    let mut value = match serde_json::to_value(p) {
        Ok(v) => v,
        Err(_) => serde_json::json!({ "threadId": "", "stale": true }),
    };
    if let Some(obj) = value.as_object_mut() {
        if let Ok(ver) = serde_json::to_value(p.projection_version_of(instance)) {
            obj.insert("projectionVersion".to_string(), ver);
        }
    }
    value
}

fn projection_event(name: &str, p: &ThreadProjection, instance: &str) -> sse::Event {
    let data = projection_json_with_version(p, instance).to_string();
    sse::Event::default().event(name).data(data)
}

// ============================================================
// 端点：GET /codex/action
// ============================================================

/// GET /m/api/v1/codex/action?action_id=：仅当前设备自己的动作（store.get_for_device）；
/// 他人动作 / 不存在 → 404（不给存在性预言机）。
pub async fn action_status(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(action_id) = params
        .get("action_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let view = st
        .codex_control
        .router
        .store()
        .get_for_device(&action_id, &device_id);
    match view {
        Ok(Some(v)) => json_no_store(
            StatusCode::OK,
            serde_json::to_value(&v).unwrap_or_else(|_| serde_json::json!({})),
        ),
        Ok(None) => error_body(StatusCode::NOT_FOUND, "no_action", None),
        Err(e) => {
            log::error!("codex/action 查询失败: {e}");
            json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            )
        }
    }
}

// ============================================================
// S4 端点：GET /codex/action/verify（只读核对）+ POST /codex/action/manual-resume
// （人工继续）——设计 §6 全部契约的 HTTP 面
// ============================================================

/// S4 只读核对核心（设计 §6.1）：只针对原目标线程和动作，顺序 =
/// ① 读持久动作已有结论 → ② 收敛有效迟到回执（S3 对账兜底消费口，证据驱动 CAS）
/// → ③ 从新鲜目标状态读原消息的**正式回显**（`userMessage.clientId == 本次
/// client_message_id`）。单次核对 ≤10 秒（可提前返回）；**零发送 mutation**
///（无 start-turn/respond/interrupt 帧；收敛写 = 账本 CAS 与屏障解除除外）。
///
/// 结果表（设计 §6.1）：
/// * 明确零投递（NotSent 终态 / cancelled）→ `not_delivered`；
/// * 找到原消息/有效受理回执 → `delivered`（收敛 accepted 并绑 Turn；运行顺序
///   限制按绑定 Turn 实时状态——绑定 Turn 终态 + 新鲜 idle 才解除，不立即 flush）；
/// * accepted 但始终无 turnId 且无法补绑 → `accepted_unbound`（「消息已受理，
///   执行状态待确认」；显式解除运行屏障的出口对它开放）；
/// * 无证据/快照缺失/查不到 → `unresolved`（**「没找到」≠「没发送」**，屏障保留）。
async fn verify_action_core(
    hub: &CodexControlHub,
    key: &ThreadKey,
    action_id: &str,
    cmid: &str,
) -> CodexActionVerifyView {
    let verified_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let store = hub.router.store();
    let mut released = false;
    // ① 读账本当前事实（核对自身的账本读；属主隔离由端点先行完成）
    let first = store.get_for_device_unisolated(action_id);
    let Some(mut cur) = first else {
        return CodexActionVerifyView {
            action_id: action_id.to_string(),
            stage: crate::services::codex_control::DeliveryStage::Settled,
            outcome: CodexVerifyOutcome::Unresolved,
            turn_id: None,
            run_limit_released: false,
            reason_code: None,
            verified_at,
        };
    };
    if matches!(cur.state, ActionState::Rejected | ActionState::Cancelled) {
        return CodexActionVerifyView {
            action_id: action_id.to_string(),
            stage: cur.stage,
            outcome: CodexVerifyOutcome::NotDelivered,
            turn_id: None,
            run_limit_released: false,
            reason_code: cur.receipt.as_ref().and_then(|r| r.reason_code.clone()),
            verified_at,
        };
    }
    // ② 收敛**本动作**的有效迟到回执（Gate B 复审：定向消费——异动作的成熟证据
    // 原样留给调度循环对账，核对的 ≤10 秒预算不被异动作的回显等待软化；归账
    // 单点与兜底同源 [`converge_one_late_result`]，DAO CAS 保证只收敛一次）。
    // 预算自步骤②起贯穿：②的回显等待与③的 ensure 共用同一绝对截止。
    let deadline =
        crate::services::codex_desktop_ipc::budget::Deadline::after_ms(hub.verify_budget_ms());
    for late in hub.adapter.take_ready_late_results_for(action_id).await {
        converge_one_late_result(hub, late, deadline).await;
    }
    if let Some(v) = store.get_for_device_unisolated(action_id) {
        cur = v;
    }
    // ③ 新鲜目标状态（预算内 ensure；失败/超时 = 快照缺失，按「无证据」保守）
    let adapter = hub.adapter.clone();
    let key2 = key.clone();
    let ensure_fut = async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            adapter.ensure_thread(&key2),
        )
        .await
    };
    let projection = match deadline.within(ensure_fut).await {
        Ok(Ok(Ok(p))) => Some(p),
        _ => None,
    };
    let fresh = projection.as_ref().is_some_and(|p| !p.stale);
    let bound_turn = store.bound_turn_id(action_id);
    match cur.state {
        // 已受理：绑 Turn 的查终态证据释放运行限制；未绑的先试补绑
        ActionState::Accepted => {
            if let Some(turn) = bound_turn {
                // 运行顺序限制按绑定 Turn 实时状态：终态证据 + 新鲜 idle 才解除
                if fresh {
                    let p = projection.clone().unwrap();
                    released = hub.router.release_turn_if_complete(key, &p);
                }
                CodexActionVerifyView {
                    action_id: action_id.to_string(),
                    stage: cur.stage,
                    outcome: CodexVerifyOutcome::Delivered,
                    turn_id: Some(turn),
                    run_limit_released: released,
                    reason_code: cur.receipt.as_ref().and_then(|r| r.reason_code.clone()),
                    verified_at,
                }
            } else {
                // 补绑：经原 client_message_id 与正式实体关联（不改写已受理事实）
                match rebind_from_echo(hub, key, action_id, cmid).await {
                    Some(turn) => {
                        if fresh {
                            let p = projection.clone().unwrap();
                            released = hub.router.release_turn_if_complete(key, &p);
                        }
                        CodexActionVerifyView {
                            action_id: action_id.to_string(),
                            stage: cur.stage,
                            outcome: CodexVerifyOutcome::Delivered,
                            turn_id: Some(turn),
                            run_limit_released: released,
                            reason_code: cur.receipt.as_ref().and_then(|r| r.reason_code.clone()),
                            verified_at,
                        }
                    }
                    None => CodexActionVerifyView {
                        action_id: action_id.to_string(),
                        stage: cur.stage,
                        outcome: CodexVerifyOutcome::AcceptedUnbound,
                        turn_id: None,
                        run_limit_released: false,
                        reason_code: cur.receipt.as_ref().and_then(|r| r.reason_code.clone()),
                        verified_at,
                    },
                }
            }
        }
        // 结果不确定（reserved/unknown）：正式回显收敛（迟到回执已在上一步收敛过）
        ActionState::Reserved | ActionState::Unknown => {
            match hub.adapter.turn_of_client_message(key, cmid).await {
                Some(turn) => {
                    // 收敛 accepted + 绑 Turn 屏障（finish_late_result 单次 CAS：
                    // accepted 绑定 Turn 身份 = 「送达/已受理」且顺序限制按 Turn 实时状态）
                    let receipt = ActionReceipt {
                        action_id: action_id.to_string(),
                        delivery: Delivery::Accepted,
                        route: crate::services::codex_control::ControlRoute::DesktopIpc,
                        turn_id: Some(turn.clone()),
                        envelope_id: None,
                        reason_code: Some(
                            crate::services::codex_control::READ_VERIFY_REASON.to_string(),
                        ),
                    };
                    let _ = store.finish_late_result(&receipt);
                    // 内存 Turn 簿记同步（补绑的身份供释放门消费）
                    hub.router.bind_turn_id(key, action_id, &turn);
                    if fresh {
                        let p = projection.clone().unwrap();
                        released = hub.router.release_turn_if_complete(key, &p);
                    }
                    let cur = store.get_for_device_unisolated(action_id).unwrap_or(cur);
                    CodexActionVerifyView {
                        action_id: action_id.to_string(),
                        stage: cur.stage,
                        outcome: CodexVerifyOutcome::Delivered,
                        turn_id: Some(turn),
                        run_limit_released: released,
                        reason_code: cur.receipt.as_ref().and_then(|r| r.reason_code.clone()),
                        verified_at,
                    }
                }
                None => CodexActionVerifyView {
                    action_id: action_id.to_string(),
                    stage: cur.stage,
                    outcome: CodexVerifyOutcome::Unresolved,
                    turn_id: bound_turn,
                    run_limit_released: false,
                    reason_code: cur.receipt.as_ref().and_then(|r| r.reason_code.clone()),
                    verified_at,
                },
            }
        }
        // 排队中：无不确定结论可收敛，如实保留（stage=queued 表达）
        ActionState::Queued => CodexActionVerifyView {
            action_id: action_id.to_string(),
            stage: cur.stage,
            outcome: CodexVerifyOutcome::Unresolved,
            turn_id: None,
            run_limit_released: false,
            reason_code: None,
            verified_at,
        },
        // ② 之后新落终态的拒绝/撤回（迟到拒绝收敛、竞态撤回）：按明确零投递呈报
        ActionState::Rejected | ActionState::Cancelled => CodexActionVerifyView {
            action_id: action_id.to_string(),
            stage: cur.stage,
            outcome: CodexVerifyOutcome::NotDelivered,
            turn_id: None,
            run_limit_released: false,
            reason_code: cur.receipt.as_ref().and_then(|r| r.reason_code.clone()),
            verified_at,
        },
    }
}

/// 补绑（accepted-unbound）：正式 userMessage 回显 → Turn 身份补进持久绑定槽位
///（`rebind_turn`：只补绑定槽位不改写已受理事实）+ 内存簿记同步。多义/无回显 → None。
async fn rebind_from_echo(
    hub: &CodexControlHub,
    key: &ThreadKey,
    action_id: &str,
    cmid: &str,
) -> Option<String> {
    let turn = hub.adapter.turn_of_client_message(key, cmid).await?;
    if hub
        .router
        .store()
        .rebind_turn(action_id, &turn)
        .unwrap_or(false)
    {
        hub.router.bind_turn_id(key, action_id, &turn);
    }
    Some(turn)
}

/// GET /m/api/v1/codex/action/verify?action_id=：**只读核对**（设计 §6.1）。
/// 仅本设备的动作（他人/不存在 → 404，与查询同语义）；零发送 mutation；
/// ≤10 秒有界返回（可提前返回）；「没找到」= unresolved 200（不是错误）。
pub async fn action_verify(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(action_id) = params
        .get("action_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let hub = &st.codex_control;
    let store = hub.router.store();
    // 属主隔离（与 GET /codex/action 同语义：非属主/不存在 → 404，不给存在性预言机）
    let view = match store.get_for_device(&action_id, &device_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/action/verify 查询失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    if view.is_none() {
        return error_body(StatusCode::NOT_FOUND, "no_action", None);
    }
    let requested_action_id = action_id.clone();
    let action_id = match store.canonical_batch_action_id(&action_id) {
        Ok(id) => id,
        Err(_) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
    };
    let Some((thread_id, cmid)) = store.action_thread_and_cmid(&action_id) else {
        return error_body(StatusCode::NOT_FOUND, "no_action", None);
    };
    let key = local_key(&thread_id);
    let v = verify_action_core(hub, &key, &action_id, &cmid).await;
    if v.run_limit_released {
        hub.notify_settled(&key);
    }
    // G09：组答题的逐题确认事实——只读投影里已 Answered 的成员（不把 Accepted
    // 当全部完成）。非组动作返回空数组。
    let projection = hub.adapter.projection(&key).await;
    let claimed_keys = store
        .question_keys_for_action(&action_id)
        .unwrap_or_default();
    // G09：按**来源组**收集已正式回答的成员（含用户复核跳过、因此不在本次
    // claim 里的已答题），否则部分确认永远无法收敛。
    let source = claimed_keys.iter().find_map(|question_key| {
        projection
            .async_questions
            .iter()
            .find(|question| question.question_key == *question_key)
            .map(|question| {
                (
                    question.source_turn_id.clone(),
                    question.source_item_id.clone(),
                )
            })
    });
    let answered_question_keys: Vec<String> = match source {
        Some((source_turn_id, source_item_id)) => projection
            .async_questions
            .iter()
            .filter(|question| {
                question.source_turn_id == source_turn_id
                    && question.source_item_id == source_item_id
                    && question.state
                        == crate::services::codex_desktop_ipc::async_question::InteractionState::Answered
            })
            .map(|question| question.question_key.clone())
            .collect(),
        None => claimed_keys
            .iter()
            .filter(|question_key| {
                projection.async_questions.iter().any(|question| {
                    question.question_key == **question_key
                        && question.state
                            == crate::services::codex_desktop_ipc::async_question::InteractionState::Answered
                })
            })
            .cloned()
            .collect(),
    };
    let mut value =
        serde_json::to_value(&v).unwrap_or_else(|_| serde_json::json!({ "outcome": "unresolved" }));
    if let Some(object) = value.as_object_mut() {
        object.insert("actionId".into(), serde_json::json!(requested_action_id));
        object.insert(
            "answeredQuestionKeys".into(),
            serde_json::json!(answered_question_keys),
        );
    }
    json_no_store(StatusCode::OK, value)
}

/// POST /codex/action/manual-resume 请求体（camelCase；deny_unknown_fields 与同族
/// 端点同纪律）。`text` 必须与原动作正文一致（服务器权威——后继重发的就是原消息）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManualResumeReq {
    pub action_id: String,
    pub new_action_id: String,
    pub text: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// POST /m/api/v1/codex/action/manual-resume：**明确人工继续**（设计 §6.2）——
/// 只读核对仍无结论时的知情出口：撤销旧动作尚未取得的首次写出权 + 解除屏障 +
/// 建唯一后继（**同一持久事务**），后继随即走正常发送路径（目标/pending/打断/忙态/
/// 队列守卫全部同源，不强行穿透审批、不转无头）。
///
/// * 幂等：相同请求/并发第二台设备 → `already_resumed`（既有后继 + 最新状态）；
/// * 确认期间已收敛 → `converged`（返回最新结果，不按过期确认重发）；
/// * 原 actionId 的 Unknown 事实不改写；同 ID 异载荷 → 409；
/// * 「本操作不是证明原消息未送达、再次发送会产生新消息」由客户端文案披露，
///   服务端只留痕（原 actionId、解除时间与原因、新 actionId 关联 + 审计）。
pub async fn action_manual_resume(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<ManualResumeReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let action_id = req.action_id.trim().to_string();
    let new_action_id = req.new_action_id.trim().to_string();
    let text = req.text.trim().to_string();
    let reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if action_id.is_empty() || new_action_id.is_empty() || text.is_empty() {
        return bad_request();
    }
    let Some((device_id, device_name)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let hub = &st.codex_control;
    // 静态门（与 session-send 同源单点）：不可尝试 → 诚实拒绝（事务前拒绝，零写入）
    if let Err(code) = hub.send_gate_static() {
        let (status, reason) = static_gate_refusal(code);
        return json_no_store(
            status,
            serde_json::json!({ "error": code, "reasonCode": code, "reason": reason }),
        );
    }
    let store = hub.router.store();
    // 合并发送只有一个实际 start-turn；旧客户端的单条知情重发不代表授权重发整批。
    match store.get_for_device(&action_id, &device_id) {
        Ok(Some(_)) => {}
        Ok(None) => return error_body(StatusCode::NOT_FOUND, "no_action", None),
        Err(_) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
    }
    let canonical = match store.canonical_batch_action_id(&action_id) {
        Ok(id) => id,
        Err(_) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
    };
    match store.batch_wire_text(&canonical) {
        Ok(Some(_)) => return error_body(StatusCode::CONFLICT, "batch_requires_verification", Some("CODEX_BATCH_REQUIRES_VERIFICATION: 此消息已合并发送，请先在桌面核对整批结果，不能按单条重发")),
        Ok(None) => {},
        Err(_) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
    }
    let resumed = store.manual_resume(
        &action_id,
        &device_id,
        &device_name,
        &new_action_id,
        &text,
        reason,
    );
    match resumed {
        Ok(crate::services::codex_control::ManualResume::Resumed {
            view,
            successor_action_id,
            thread_id,
        }) => {
            let key = local_key(&thread_id);
            // 持久屏障已解除：同步清内存 Turn 顺序限制（本进程内队列立即恢复）
            hub.router.force_clear_turn_limit(&key, &action_id);
            // 后继走**正常发送路径**（全部守卫同源；前台预算同 session-send）
            let ctx = ActionContext {
                action_id: successor_action_id.clone(),
                device_id: device_id.clone(),
                device_name,
                kind: ActionKind::Send,
                key: key.clone(),
            };
            let front = crate::services::codex_desktop_ipc::budget::Deadline::after_ms(
                hub.front_budget_ms(),
            );
            let compat_attempt =
                hub.compat_send_allowed(st.store.with(super::auto_adapt_enabled_conn));
            let out = hub
                .router
                .send_bounded(ctx.clone(), text, false, false, front)
                .await;
            let send_payload = match &out {
                RouterOutcome::Completed(receipt) => {
                    // 后继是一次真实发送尝试：兼容证据记账与 session-send 同判据同源
                    record_send_outcome_compat_evidence(&st, receipt, compat_attempt).await;
                    codex_audit(
                        st.as_ref(),
                        &ctx,
                        AUDIT_SEND,
                        &thread_id,
                        receipt.envelope_id.as_deref(),
                        None,
                        &audit_result_of_receipt(receipt),
                    );
                    serde_json::json!({ "receipt": receipt })
                }
                RouterOutcome::Queued { position, .. } => {
                    codex_audit(
                        st.as_ref(),
                        &ctx,
                        AUDIT_SEND,
                        &thread_id,
                        None,
                        None,
                        "queued",
                    );
                    serde_json::json!({ "queued": true, "position": position })
                }
            };
            log::info!(
                "codex_ipc: 人工继续（撤销旧写出权 + 解除屏障 + 唯一后继）old={action_id} successor={successor_action_id} device={device_id}"
            );
            json_no_store(
                StatusCode::OK,
                serde_json::json!({
                    "status": "resumed",
                    "predecessor": view,
                    "successorActionId": successor_action_id,
                    "send": send_payload,
                }),
            )
        }
        Ok(crate::services::codex_control::ManualResume::Converged { view }) => json_no_store(
            StatusCode::OK,
            serde_json::json!({ "status": "converged", "action": view }),
        ),
        // 复审 R-T1：写右已被 writer 取得（claim 胜出）且**派发尝试仍在途**（动作
        // 未收尾）——不建后继不重发。复审 round 4 V2：独立状态 `write_right_in_flight`
        //（**不是** converged——converged 只能表达真实终态；「已可能写出但未收尾」
        // 是设计 §5.2 的待确认面，前端保持待确认，稍后可再核对/收尾后人工继续）。
        Ok(crate::services::codex_control::ManualResume::WriteRightObtained { view }) => {
            json_no_store(
                StatusCode::OK,
                serde_json::json!({ "status": "write_right_in_flight", "action": view }),
            )
        }
        Ok(crate::services::codex_control::ManualResume::AlreadyResumed {
            view,
            successor_action_id,
        }) => json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "status": "already_resumed",
                "predecessor": view,
                "successorActionId": successor_action_id,
            }),
        ),
        Ok(crate::services::codex_control::ManualResume::NotFound) => {
            error_body(StatusCode::NOT_FOUND, "no_action", None)
        }
        Ok(crate::services::codex_control::ManualResume::Queued { .. }) => error_body(
            StatusCode::CONFLICT,
            "queued_use_retract",
            // 与 CODEX_QUEUE_RETRACT_PENDING 的 reason 区分（错误码对账「同句多码」纪律）；
            // 文案与 locale `errors.codexManualResumeQueued` 同义。
            Some("CODEX_MANUAL_RESUME_QUEUED: 消息仍在排队，无法人工继续——请使用撤回"),
        ),
        Ok(crate::services::codex_control::ManualResume::TextMismatch { .. }) => error_body(
            StatusCode::CONFLICT,
            "manual_resume_text_mismatch",
            Some("CODEX_MANUAL_RESUME_TEXT_MISMATCH: 正文与原动作不一致（后继重发的就是原消息）"),
        ),
        Err(crate::services::codex_control::StoreError::Conflict) => {
            error_body(StatusCode::CONFLICT, "action_conflict", None)
        }
        Err(e) => {
            log::error!("codex/action/manual-resume 失败: {e}");
            json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            )
        }
    }
}

/// 兼容尝试的证据记账（session-send 与人工继续后继**同一判据同一份**；S3：
/// 迟到收敛回执不写探测账本——旧连接的真实回执不得当成新连接的能力证据）。
async fn record_send_outcome_compat_evidence(
    st: &Arc<RemoteState>,
    receipt: &ActionReceipt,
    compat_attempt: bool,
) {
    if !compat_attempt {
        return;
    }
    let late_receipt =
        receipt.reason_code.as_deref() == Some(crate::services::codex_control::LATE_RESULT_REASON);
    match (receipt.delivery, late_receipt) {
        (_, true) => {}
        (Delivery::Accepted, false) => {
            record_compat_send_outcome(st, CompatSendOutcome::Accepted).await
        }
        (Delivery::UnknownDelivery, false) => {
            record_compat_send_outcome(st, CompatSendOutcome::Unknown).await
        }
        (Delivery::NotSent, false) => {
            record_compat_send_outcome(st, CompatSendOutcome::NotSent).await
        }
        (Delivery::Rejected, false) => {}
    }
}

// ============================================================
// 端点：GET /codex/queue + POST /codex/queue/retract
// ============================================================

/// GET /m/api/v1/codex/queue?session_id=：CodexQueueView——当前设备在该 Thread 的
/// 待发列表（position = 跨设备 FIFO 实际位置）；compat fence 在场 →
/// blockedReasonCode=compat_outstanding。
pub async fn queue_view(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/queue 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    let store = st.codex_control.router.store();
    let actions: Vec<CodexActionView> = match store.list_queued(&key, &device_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue 读取失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    // 阻塞原因三态派生（复审 P2-3）：native_turn_pending = Accepted 且绑定 turnId
    //（终态证据驱动自动对账，等待后自动继续）；native_delivery_unknown = 原生
    // Unknown/缺 turnId 保守屏障（**无自动恢复路径**，如实说）；compat_outstanding =
    // 兼容路径 fence。都不提示不存在的「回 Desktop 手工解除」。
    let fence_summary = store.outstanding_fence_summary(&key);
    let blocked_reason_code = match &fence_summary {
        Ok(rows) => {
            // 优先级：unknown/缺 turnId（**永不自动解除**）> accepted+turnId（等待
            // 后自动继续）> compat。若两者并存，队列最终仍会停在 Unknown 屏障上，
            // 「自动继续」就是失实承诺——如实优先展示无自动恢复路径。
            let native_unknown = rows
                .iter()
                .any(|(r, s, tid)| r == "desktop_ipc" && (*s != "accepted" || !*tid));
            let native_pending = rows
                .iter()
                .any(|(r, s, tid)| r == "desktop_ipc" && s == "accepted" && *tid);
            if native_unknown {
                Some(BLOCKED_NATIVE_DELIVERY_UNKNOWN.to_string())
            } else if native_pending {
                Some(BLOCKED_NATIVE_TURN_PENDING.to_string())
            } else if !rows.is_empty() {
                Some(BLOCKED_COMPAT_OUTSTANDING.to_string())
            } else {
                None
            }
        }
        Err(e) => {
            log::error!("codex/queue fence 摘要读取失败: {e}");
            None
        }
    };
    // 原生活动回合（2026-10-22 扩展）：无屏障但镜像里有 inProgress 回合（桌面
    // 手打的原生回合）→ native_turn_active，打断出口对它同样开放。屏障态优先
    // （native_turn_pending / unknown / compat 语义不变）；**仅当 fence 摘要读取
    // 成功且确认无屏障**才回退——读降级（Err）期真实阻塞可能正是读不出的屏障，
    // 不得被镜像活动回合「洗」成 native_turn_active（复审 Minor-2）；镜像 stale /
    // 无活动回合 → 保持 None（面板自隐，不渲染空壳）。
    let blocked_reason_code = if blocked_reason_code.is_none() && fence_summary.is_ok() {
        st.codex_control
            .adapter
            .mirror_active_turn_id(&key)
            .await
            .map(|_| BLOCKED_NATIVE_TURN_ACTIVE.to_string())
    } else {
        blocked_reason_code
    };
    // 在册屏障动作清单（2026-10-19 无出口屏障修复）：人工解除端点的靶向依据——
    // 前端按钮据此逐个 force-continue。读取失败如实降级为空清单（阻塞码派生同上，
    // 两者独立读取互不牵连）。
    let barrier_action_ids = match store.barrier_action_ids(&key) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue 屏障动作清单读取失败: {e}");
            Vec::new()
        }
    };
    // S2：持久暂停状态随视图下发（前端渲染「队列已暂停 + 恢复/先发送」入口）。
    let (queue_paused, queue_pause_reason) = match store.queue_pause(&key) {
        Ok(Some(p)) => (true, Some(p.pause_reason)),
        Ok(None) => (false, None),
        Err(e) => {
            log::error!("codex/queue 暂停状态读取失败: {e}");
            (false, None)
        }
    };
    let view = CodexQueueView {
        actions,
        blocked_reason_code,
        barrier_action_ids,
        // interrupt 能力位随队列视图下发（复审 P2-2）：无能力 build 的打断按钮在
        // 前端源头隐藏，而不是渲染出来永远 503。读当前连接的动态 profile——与
        // adapter 各写前门控同源。
        // T06：打断入口与写侧同源（逐操作策略）。**实际语义（2026-10-29 修正）**：
        // 策略对 Interrupt 只门证据/适配/目标在场，**不以「有无活动回合」作门**——
        // 可打断性由 expectedTurnId 安全阀承载（与写侧 interrupt_verdict 同一口径）。
        can_interrupt: st
            .codex_control
            .adapter
            .effective_operations(&key)
            .await
            .allows(crate::services::codex_desktop_ipc::capability::CodexOperation::Interrupt),
        queue_paused,
        queue_pause_reason,
    };
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({ "actions": [] })),
    )
}

/// POST /codex/queue/retract 请求体（camelCase）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRetractReq {
    #[serde(default)]
    pub action_id: String,
}

/// POST /m/api/v1/codex/queue/retract：`{actionId}`——仅本设备 queued 可撤；成功回
/// cancelled 视图；已派发（reserved/终态）/ 撤回竞态 → 409；不存在 / 他人动作 → 404；
/// 墓碑保留（cancelled actionId 重放 → 409，不重发）。
pub async fn queue_retract(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<QueueRetractReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let action_id = req.action_id.trim().to_string();
    if action_id.is_empty() {
        return bad_request();
    }
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let store = st.codex_control.router.store();
    // 先查属主（404 语义），再 CAS（409 语义）
    let view = match store.get_for_device(&action_id, &device_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue/retract 查询失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    let Some(view) = view else {
        return error_body(StatusCode::NOT_FOUND, "no_action", None);
    };
    match store.retract(&action_id, &device_id) {
        Ok(true) => {
            let view = store
                .get_for_device(&action_id, &device_id)
                .ok()
                .flatten()
                .unwrap_or(view);
            json_no_store(
                StatusCode::OK,
                serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
            )
        }
        // false = CAS 失败：已派发（reserved/终态）或与派发竞态 → 409（保留墓碑）
        Ok(false) => error_body(StatusCode::CONFLICT, "action_conflict", None),
        Err(e) => {
            log::error!("codex/queue/retract 撤回失败: {e}");
            json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            )
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueEditReq {
    pub session_id: String,
    pub action_id: String,
}

/// 编辑必须先赢得撤回 CAS，再返回同设备同线程的普通消息正文。
pub async fn queue_edit(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<QueueEditReq>, JsonRejection>,
) -> Response {
    let Ok(Json(req)) = body else {
        return bad_request();
    };
    let action_id = req.action_id.trim();
    let session_id = req.session_id.trim();
    if action_id.is_empty() || session_id.is_empty() {
        return bad_request();
    }
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let sid = session_id.to_string();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &sid)).await {
            Ok(Some(v)) => v,
            Ok(None) => return not_found(),
            Err(_) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
        };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    let store = st.codex_control.router.store();
    match store.get_for_device(action_id, &device_id) {
        Ok(Some(_))
            if store
                .action_thread_and_cmid(action_id)
                .is_some_and(|(tid, _)| tid == key.thread_id) => {}
        Ok(_) => return error_body(StatusCode::NOT_FOUND, "no_action", None),
        Err(_) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
    }
    match store.retract_for_edit(action_id, &device_id, &key) {
        Ok(Some(text)) => json_no_store(
            StatusCode::OK,
            serde_json::json!({"actionId":action_id,"text":text}),
        ),
        Ok(None) => error_body(StatusCode::CONFLICT, "action_conflict", None),
        Err(_) => error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
    }
}

// ============================================================
// 端点：POST /codex/queue/force-continue（2026-10-19 无出口屏障修复）
// ============================================================

/// POST /codex/queue/force-continue 请求体（camelCase）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForceContinueReq {
    #[serde(default)]
    pub action_id: String,
}

/// POST /m/api/v1/codex/queue/force-continue：`{actionId}`——**人工解除「无出口保守
/// 屏障」**（PR #165 复审 Important：native 投递 unknown 的队列屏障只有两处自动出口
/// （turn 终态证据 / reconcile 三证据），route=desktop_ipc 且 state=unknown（尤其缺
/// turnId）的屏障两处都到不了，队列永久阻塞）。语义：用户确认该消息实际去向已由其
/// 在桌面端核对，显式解除队列屏障（copy 保持诚实——`native_delivery_unknown` 的
/// 「无自动恢复路径」语义不变，人工解除是显式覆盖，前端二次确认披露重复投递风险）。
///
/// - 鉴权：gated nest 内层 gate 结构性覆盖 + 属主设备隔离（`get_for_device` 同款——
///   非属主/不存在一律 404，不给存在性预言机）；
/// - 状态机（CAS 安全）：仅解除「屏障在册且已终态」的行（unknown/accepted）；
///   queued → 409 `queued_use_retract`（指引走既有撤回）；屏障在册但不在本端点
///   范围（reserved 在飞 fence），或 compat 兜底回合在飞（fence 尚未武装的瞬态
///   窗口）——bridge 落账 / 对账有自动路径 → 409 `barrier_in_flight`（终审 Minor：
///   不得谎报 `no_barrier` 误导排障）；屏障已被自动路径解除/从未在册 → 409
///   `no_barrier`；
/// - 幂等：重复调用返回当前状态（200），绝不重复写审计；
/// - 单调：解除写 `barrier_force_released_at` 标记，全部置屏障写点带同名单调性守卫；
/// - 审计：解除成功单条 write_audit（action=`force_continue`）；
/// - 解除后清该 Thread 的内存 Turn 顺序限制并接力 flush（本进程内队列立即恢复）。
pub async fn queue_force_continue(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<ForceContinueReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let action_id = req.action_id.trim().to_string();
    if action_id.is_empty() {
        return bad_request();
    }
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let store = st.codex_control.router.store();
    match store.get_for_device(&action_id, &device_id) {
        Ok(Some(_)) => {}
        Ok(None) => return error_body(StatusCode::NOT_FOUND, "no_action", None),
        Err(_) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
    }
    // 成员共享同一实际发送的屏障；属主核对在重定向之前完成。
    let action_id = match store.canonical_batch_action_id(&action_id) {
        Ok(id) => id,
        Err(_) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal", None),
    };
    // S4 前置（设计 §6.1「已受理无 turnId 的显式解除……仍要求新鲜目标状态可输入、
    // 无 pending」）：accepted 且**未绑 Turn** 的行（本出口的主体）解除前必须经
    // 新鲜判定——pending 在场或状态不可确认 → 409 不解除（屏障保留）。unknown 行
    // 与 accepted-with-turn 行不在此列（2026-10-19 契约与自动路径语义不变）。
    if let Ok(Some(view)) = store.get_for_device(&action_id, &device_id) {
        if view.state == ActionState::Accepted && store.bound_turn_id(&action_id).is_none() {
            let hub = &st.codex_control;
            let key = local_key(&action_thread_id(store, &action_id));
            let gate = fresh_send_gate(hub, &key).await;
            if gate != SendGate::Clear {
                return error_body(
                    StatusCode::CONFLICT,
                    "target_not_inputtable",
                    Some("CODEX_TARGET_NOT_INPUTTABLE: 目标状态不可输入（有未处理原生请求或状态无法确认），请先处理后再解除"),
                );
            }
        }
    }
    let release = match store.force_release_barrier(&action_id, &device_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue/force-continue 解除失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    match release {
        BarrierRelease::NotFound => error_body(StatusCode::NOT_FOUND, "no_action", None),
        BarrierRelease::Released { view, thread_id } => {
            // 持久屏障已清：同步清内存 Turn 顺序限制并接力下一条（否则本进程内队列
            // 仍被 is_turn_active / flush 让路逻辑挡住，人工解除只在重启后生效）
            let key = local_key(&thread_id);
            st.codex_control
                .router
                .force_clear_turn_limit(&key, &action_id);
            st.codex_control.notify_settled(&key);
            log::info!(
                "codex_ipc: 人工解除队列屏障（桌面端核对后显式覆盖）action={action_id} thread={thread_id}"
            );
            action_view_response(view)
        }
        // 幂等命中：直接返回当前状态（200），零新写入零审计
        BarrierRelease::AlreadyReleased { view, .. } => action_view_response(view),
        // 不在人工解除范围：按「是否将有/已有自动落账路径」区分——屏障在册，或
        // compat 兜底回合在飞（fence 尚未武装的瞬态窗口），都回 barrier_in_flight
        // 如实说明；真正无屏障可解才是 no_barrier
        BarrierRelease::NotEligible {
            view,
            barrier_in_register,
        } => {
            if view.state == ActionState::Queued {
                error_body(
                    StatusCode::CONFLICT,
                    "queued_use_retract",
                    Some("CODEX_QUEUE_RETRACT_PENDING: 该消息仍在排队，请使用撤回"),
                )
            } else if barrier_in_register {
                error_body(
                    StatusCode::CONFLICT,
                    "barrier_in_flight",
                    Some("CODEX_QUEUE_FORCE_IN_FLIGHT: 该消息派发在飞（有自动落账路径），不在人工解除范围"),
                )
            } else {
                error_body(StatusCode::CONFLICT, "no_barrier", None)
            }
        }
    }
}

// ============================================================
// 端点：POST /codex/queue/resume（S2：打断后队列的唯一恢复出口）
// ============================================================

/// POST /codex/queue/resume 请求体（camelCase；deny_unknown_fields 与同族端点同纪律）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueResumeReq {
    pub session_id: String,
}

/// POST /m/api/v1/codex/queue/resume：**用户显式「继续发送」**——清除该 Thread 的
/// 打断后持久暂停（`codex_queue_pause` 行），让调度循环按既有 FIFO 接力旧队列。
/// 这是暂停的**唯一**解除出口（设计 §4.3「只有用户明确『继续发送』才恢复」）。
///
/// - 鉴权：gated nest 内层 gate 结构性覆盖 + 设备 cookie；会话/thread 不可解析 → 404；
/// - 幂等：未暂停时同样 200 `{ok:true, paused:false}`（零写入零审计）；
/// - 解除成功：单条 write_audit（action=`codex_queue_resume`）+ notify_settled 接力。
pub async fn queue_resume(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<QueueResumeReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let session_id = req.session_id.trim().to_string();
    if session_id.is_empty() {
        return bad_request();
    }
    let Some((device_id, device_name)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = session_id.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/queue/resume 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    let store = st.codex_control.router.store();
    match store.clear_queue_pause(&key) {
        Ok(true) => {
            // 审计 ctx 的 kind 仅落 ActionContext 形参（resume 不写动作账本，
            // summary 不含 kind）；用 Interrupt 表达「队列控制动作」语义最近。
            codex_audit(
                st.as_ref(),
                &ActionContext {
                    action_id: format!("queue-resume-{}", key.thread_id),
                    device_id,
                    device_name,
                    kind: ActionKind::Interrupt,
                    key: key.clone(),
                },
                "codex_queue_resume",
                &session.id,
                None,
                None,
                "resumed",
            );
            st.codex_control.notify_settled(&key);
            // 复审 R-T4：恢复也是暂停状态变化——重发布该线程投影（queuePaused
            // 翻转帧；settle 接力照常）
            st.codex_control
                .adapter
                .republish_thread_projection(&key)
                .await;
            log::info!("codex_ipc: 用户显式恢复队列 thread={}", key.thread_id);
            json_no_store(
                StatusCode::OK,
                serde_json::json!({ "ok": true, "paused": false }),
            )
        }
        Ok(false) => json_no_store(
            StatusCode::OK,
            serde_json::json!({ "ok": true, "paused": false }),
        ),
        Err(e) => {
            log::error!("codex/queue/resume 解除暂停失败: {e}");
            json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            )
        }
    }
}

/// S4：新鲜发送门判定（force-continue 的 accepted-unbound 前置；与
/// AdapterDispatch::ensure_and_gate 同源分类，但失败不 unfollow——解除路径不改变
/// 关注状态）。有界（ensure 10s 上限 + 核对预算），失败/超时 = StateUnknown（保守拒解）。
async fn fresh_send_gate(hub: &CodexControlHub, key: &ThreadKey) -> SendGate {
    let adapter = hub.adapter.clone();
    let k = key.clone();
    let fut = async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            adapter.ensure_thread(&k),
        )
        .await
    };
    let deadline =
        crate::services::codex_desktop_ipc::budget::Deadline::after_ms(hub.verify_budget_ms());
    match deadline.within(fut).await {
        Ok(Ok(Ok(p))) => classify_send_gate(&p),
        _ => SendGate::StateUnknown,
    }
}

/// 动作的 thread_id（单点读；读取失败保守回空——调用方 ensure 会对空 thread 保守
/// 判 StateUnknown 拒解，不会误放行）。
fn action_thread_id(
    store: &crate::services::codex_control::ActionStore,
    action_id: &str,
) -> String {
    store
        .action_thread_and_cmid(action_id)
        .map(|(t, _)| t)
        .unwrap_or_default()
}

/// 动作视图 → 200 no-store 响应。
fn action_view_response(view: CodexActionView) -> Response {
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
    )
}

// ============================================================
// 端点：POST /codex/turn/interrupt
// ============================================================

/// POST /codex/turn/interrupt 请求体（camelCase）。**deny_unknown_fields**：与
/// pending-response 同纪律——`ownerClientId` / raw method 等任何多余键 → 400
///（HTTP 层不出现 IPC owner 参数，2026-10-22 复审 P3-2）。`turnId` 可选：缺省取
/// **在册原生屏障绑定的 turnId**（上一条被桌面受理的回合），作为 Desktop 侧
/// `expectedTurnId` 安全阀——与当前活动回合不符时 Desktop 静默不动作（不误伤）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InterruptTurnReq {
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    /// T06：前台剩余预算（毫秒；省略 = 30_000，上限夹到 30_000）。
    #[serde(default, rename = "frontBudgetMs")]
    pub front_budget_ms: Option<u64>,
}

/// 打断指令的 NotSent 拒绝 → HTTP 回应（复审 P2-2：拒绝类错误必须带中文可读
/// reason，不裸示机器码；stale_context 是状态冲突 → 409，其余未送达 → 503）。
/// `NotSent` = 写尝试开始前即失败，桌面侧零改动——reason 文案如实说「未送达」。
fn interrupt_not_sent_response(reason: &str) -> Response {
    match reason {
        "stale_context" => error_body(
            StatusCode::CONFLICT,
            reason,
            Some("CODEX_INTERRUPT_STALE_CONTEXT: 桌面会话上下文已过期（桌面端可能重开或换代），请刷新页面后重试"),
        ),
        "capability_unavailable" => error_body(
            StatusCode::SERVICE_UNAVAILABLE,
            reason,
            Some("CODEX_INTERRUPT_CAPABILITY_UNVERIFIED: 当前桌面 Codex 版本未启用远程停止能力（未完成活体验证），无法远程打断回合"),
        ),
        "expected_turn_missing" => error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            reason,
            Some(
                "CODEX_INTERRUPT_VALVE_MISSING: 服务端未能取到打断安全阀（expectedTurnId），已放弃本次打断——未向桌面发送任何指令",
            ),
        ),
        _ => error_body(
            StatusCode::SERVICE_UNAVAILABLE,
            reason,
            Some("CODEX_INTERRUPT_UNDELIVERED: 停止指令未能送达桌面（连接不可用或通道拥塞），桌面未被改动"),
        ),
    }
}

/// T06：打断编排核心（/codex/turn/interrupt 与 /codex/queue/prioritize 共用）——
/// 阀值解析（请求值 → 在册原生屏障 → 镜像活动回合）→ 逐操作能力门 → 终审 F3
/// 前置暂停（写失败即放弃、零帧）→ 有界打断 → 按 delivery 收归/撤回**本次 token**
/// 的暂停行。HTTP 响应与审计映射由调用方各自完成；`Err(ready)` = 前置失败且响应
/// 已就绪（阀值缺失 409 + refused 审计 / 能力门 503 / 暂停写失败 500），全部零 IPC 帧。
///
/// 2026-10-29 自 interrupt_turn 原实现逐字抽取（T06 复用纪律：不复制调度/守卫链）：
/// - `expected_turn_id` 三路取不到 → 409 拒发 + `refused:no_expected_turn` 审计；
///   DB 读失败 → 500（本臂**有意不留审计**——审计与账本同库，读失败时再写会把
///   原始故障掩盖成审计成功，复审 Minor-1 取舍保留）；
/// - NotSent/Rejected → 只撤回本次 token 拥有的暂停行（他人 token 不动，终审 R3）；
///   撤回导致暂停态翻转时重发布投影（复审 R-T4）；清除失败保留暂停（保守不放行）；
/// - Accepted/Unknown → 保留暂停（前置档即正式档）；`queue_pause_set` 读**实际
///   持久行**（Gate C 留意项 b：并发 resume 晚于 pause_pre → 如实 false；读失败
///   保守 true，与 send/flush 门同口径）。
// `Result<_, Response>` 的 Err 分支是 axum Response（128 字节以上）。这是
// 既有 API 形状——调用方拿到 Err 就是「直接回这个响应」，Box 成指针只会在每一处
// 解引用，反而更绕。clippy 1.99 起新增 result_large_err，本函数按既有形状保留。
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
async fn stop_current_turn(
    st: &Arc<RemoteState>,
    key: &ThreadKey,
    session_id: &str,
    device_id: &str,
    device_name: &str,
    requested_turn_id: Option<String>,
    front: crate::services::codex_desktop_ipc::budget::Deadline,
) -> Result<StopTurnOutcome, Response> {
    // expectedTurnId：调用方给了就用；否则取在册原生屏障绑定的 turnId（安全阀）。
    // 复审 P1-1：两路都取不到 → 409 拒发；DB 读失败 → 500。**不得**降级为 None
    // 继续发——adapter 对 None 阀值会拒发（零 IPC 写入），这里先一步如实拒绝。
    let expected_turn_id = match requested_turn_id.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => Some(t.to_string()),
        _ => match st
            .codex_control
            .router
            .store()
            .outstanding_native_turn_id(key)
        {
            Ok(Some(v)) => Some(v),
            Ok(None) => {
                // 原生回合打断扩展（2026-10-22）：在册屏障不存在（回合由桌面
                // 手打产生）→ 回退用镜像活动回合 id 作阀值。Desktop 侧安全阀
                // 原样兜底：镜像给的 id 若已过期（回合刚结束），expectedTurnId
                // 不匹配 → 静默返回 null → 端点如实报「没有可打断回合」，
                // 失败方向保守。仍取不到 → 走下方 409 拒发（P1-1 纪律不变）。
                if let Some(active) = st.codex_control.adapter.mirror_active_turn_id(key).await {
                    Some(active)
                } else {
                    // ctx 仅用于审计（envelope 尚不存在 → None）；action_id 沿用
                    // interrupt- 前缀，result 词记拒绝原因。
                    let ctx = ActionContext {
                        action_id: "interrupt-none".to_string(),
                        device_id: device_id.to_string(),
                        device_name: device_name.to_string(),
                        kind: ActionKind::Interrupt,
                        key: key.clone(),
                    };
                    codex_audit(
                        st,
                        &ctx,
                        AUDIT_INTERRUPT,
                        session_id,
                        None,
                        None,
                        "refused:no_expected_turn",
                    );
                    return Err(error_body(
                        StatusCode::CONFLICT,
                        "no_expected_turn",
                        Some("CODEX_INTERRUPT_NO_VALVE: 取不到打断安全阀：请求未带 turnId，且没有在册的原生回合屏障，镜像里也没有正在运行的回合。为免误伤桌面其他回合，已拒绝打断（未发送任何指令）"),
                    ));
                }
            }
            Err(e) => {
                log::error!("codex/turn/interrupt 读取在册 turnId 失败: {e}");
                return Err(json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({
                        "error": "internal",
                        "reason": "CODEX_INTERRUPT_VALVE_READ_FAILED: 读取打断安全阀失败，已放弃本次打断（未向桌面发送任何指令），请稍后重试",
                    }),
                ));
            }
        },
    };
    // T06：能力门 = 逐操作策略的**证据面**（implemented 且有任一证据：契约匹配/
    // 精确 shipped/真实成功/schema）。实际语义（2026-10-29 修正过时表述）：
    // 此处不读 `allowed`、不以运行态/「有无活动回合」作门——「有无可打断的回合」
    // 由 expectedTurnId 安全阀承载。关闭自动适配/未匹配 → 503 且**零帧、零暂停**
    //（不该为不可能成功的动作冻结队列）。
    let operations = st.codex_control.adapter.effective_operations(key).await;
    let interrupt_capable = operations
        .get(crate::services::codex_desktop_ipc::capability::CodexOperation::Interrupt)
        .is_some_and(|status| {
            status.implemented
                && status.evidence
                    != crate::services::codex_desktop_ipc::capability::EvidenceSource::None
        });
    if !interrupt_capable {
        // HTTP 错误词保持稳定的 `capability_unavailable`（既有契约）；精确的逐操作
        // 原因码在 `effectiveOperations` 详情里（诊断词不进错误码位置）。
        return Err(error_body(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability_unavailable",
            Some("CODEX_INTERRUPT_CAPABILITY_UNVERIFIED: 当前桌面 Codex 版本未启用远程停止能力（未匹配契约或未关闭适配），无法远程打断回合"),
        ));
    }
    // T05：与 cancel 共用同一个**终止类前置暂停**辅助（单一出处；归属 token 语义一致）。
    // 终审 F3（Gate C 收紧）：暂停保护在**打断指令发出之前**建立（与队列派发互斥）；
    // **写失败 → 不发出打断指令**（保护建立不了就不开工），如实回 5xx。
    let (pause_token, pause_pre_inserted) = match st
        .codex_control
        // P3：暂停行绑定本次停止的目标回合——settle 按该 Turn 的终态证据自动
        // 解除停止类暂停（回合未终态 = 停止未生效，暂停保持）。
        .prepare_turn_stop_pause(key, "interrupt_pending", expected_turn_id.as_deref())
    {
        Ok(result) => result,
        Err(e) => {
            log::error!("打断前置暂停写入失败（放弃本次打断，未向桌面发送任何指令）: {e}");
            return Err(json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({
                    "error": "internal",
                    "reason": "CODEX_INTERRUPT_PAUSE_WRITE_FAILED: 队列暂停保护写入失败，已放弃本次打断（未向桌面发送任何指令），请稍后重试",
                }),
            ));
        }
    };
    // 复审 R-T4：暂停状态变化有自己的可靠传播——前置**新落行**即重发布该线程
    // 投影（local_seq 决胜穿透），手机端不依赖 Desktop 状态帧做必然触发器。
    if pause_pre_inserted {
        st.codex_control
            .adapter
            .republish_thread_projection(key)
            .await;
    }
    let outcome = st
        .codex_control
        .adapter
        // 走到这里 expected_turn_id 恒为 Some（None 两路已在上面如实拒绝）；仍以
        // Option 传入——adapter 侧的 None 硬门作为纵深防御保留（复审 P1-1）。
        .interrupt_turn_bounded(key, expected_turn_id.as_deref(), front, Some(device_id))
        .await;
    // 终审 F3：明确零投递/明确拒绝（打断未发生）→ 撤回前置暂停（本次 token 拥有
    // 的行；他人/上次受理建立的暂停不受影响，终审 R3）。Gate C #5（用户裁决）：
    // 并发 resume 在 pause_pre 落行后清行 → 本臂不回写——resume 时刻晚于 pause_pre
    // 即视为知情恢复（用户显式 resume 优先），打断继续执行、队列不自动暂停。
    // 受理/不确定 → 保留（interrupt_pending 前置档即正式档，语义 =「打断已发出，
    // 队列冻结」）；`queue_pause_set` 回执读**实际持久行**，读失败按 true 保守。
    let queue_pause_set = match outcome.delivery {
        Delivery::NotSent | Delivery::Rejected => {
            match st
                .codex_control
                .router
                .store()
                .clear_queue_pause_owned(key, &pause_token)
            {
                Ok(removed) => {
                    if removed {
                        let still = st
                            .codex_control
                            .router
                            .store()
                            .queue_pause(key)
                            .map(|p| p.is_some())
                            .unwrap_or(true);
                        if !still {
                            st.codex_control
                                .adapter
                                .republish_thread_projection(key)
                                .await;
                        }
                    }
                    false
                }
                Err(e) => {
                    log::error!("打断未发生后撤回暂停失败（保留暂停，保守不放行）: {e}");
                    true
                }
            }
        }
        Delivery::Accepted | Delivery::UnknownDelivery => st
            .codex_control
            .router
            .store()
            .queue_pause(key)
            .map(|p| p.is_some())
            .unwrap_or(true),
    };
    Ok(match outcome.delivery {
        Delivery::Accepted => StopTurnOutcome::Accepted {
            interrupted_turn_id: outcome
                .result
                .as_ref()
                .and_then(|r| r.get("interruptedTurnId"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            envelope_id: outcome.envelope_id,
            expected_turn_id: expected_turn_id.unwrap_or_default(),
            queue_pause_set,
        },
        Delivery::UnknownDelivery => StopTurnOutcome::Unknown {
            reason: outcome
                .reason_code
                .unwrap_or_else(|| "unknown_delivery".to_string()),
            envelope_id: outcome.envelope_id,
            expected_turn_id: expected_turn_id.unwrap_or_default(),
            queue_pause_set,
        },
        Delivery::Rejected => StopTurnOutcome::Rejected {
            envelope_id: outcome.envelope_id,
        },
        Delivery::NotSent => StopTurnOutcome::NotSent {
            reason: outcome
                .reason_code
                .unwrap_or_else(|| "not_sent".to_string()),
            envelope_id: outcome.envelope_id,
        },
    })
}

/// [`stop_current_turn`] 的结果（HTTP 响应与审计映射由调用方完成；审计词与响应
/// 字段保持各端点既有契约不变）。
enum StopTurnOutcome {
    /// 明确受理。`interrupted_turn_id = None` = Desktop 无匹配回合（回合已结束或
    /// expectedTurnId 不匹配的静默 no-op）——调用方**不得谎报成功**（端点如实回
    /// null），也不得当作「停止已生效」继续派发（由调用方新鲜判定把关）。
    Accepted {
        interrupted_turn_id: Option<String>,
        envelope_id: String,
        expected_turn_id: String,
        queue_pause_set: bool,
    },
    /// 帧已交 writer、结果未知（打断可能已生效）——不得诱导重按、不得继续派发。
    Unknown {
        reason: String,
        envelope_id: String,
        expected_turn_id: String,
        queue_pause_set: bool,
    },
    /// Desktop 明确拒绝。本次 token 的前置暂停已撤回。
    Rejected { envelope_id: String },
    /// 明确零投递（写尝试未开始）。本次 token 的前置暂停已撤回。
    NotSent { reason: String, envelope_id: String },
}

/// POST /codex/turn/interrupt：**打断桌面 Codex 当前回合**（`thread-follower-interrupt-turn`
/// v4）。打断后 Desktop 把该回合状态置 `interrupted`（终态）→ 镜像终态门放行 →
/// 在册屏障释放 → 待发队列接力下一条（这就是「远程点掉那个卡」的闭环）。
///
/// 语义边界（必须如实，不美化）：
/// - **破坏性**：被打断回合未完成的工作会丢失。端点只负责"发出打断"，不代用户
///   判断该不该打断——那一层由前端两步确认 + 风险披露承担。
/// - **不谎报**：回合已结束（不再 inProgress）时 Desktop 静默不动作，端点回
///   `interruptedTurnId: null` + `ok: true`——不谎称打断成功，也不报错。
/// - **投递结果三分支（复审 P2-3）**：明确受理 → 200；RPC 超时等「帧已交 writer
///   但结果未知」→ 200 + `uncertain: true`（打断可能已生效，不得报「失败」诱导
///   重按）；Desktop 明确拒绝（Rejected）→ 409（不是 503——服务本身可用）。
/// - 打断**不直接清屏障**：屏障由终态证据驱动释放（`release_turn_if_complete`），
///   本端点只发打断并触发一次结算；回合终态帧到达时调度循环自动接力。
/// - **全臂审计（复审 P3-1）**：拒绝/不确定/明确拒绝各臂都留 `codex_interrupt`
///   审计，事后可回答「谁在何时试图打断、为什么没打出去」。
pub async fn interrupt_turn(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<InterruptTurnReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    // R13：打断也吃**同一前台预算**（零预算 = 零帧）；前置暂停建立后若已到期，
    // 撤回本次暂停并如实零投递，不再追加整段派发时间。
    let front_budget_ms = req
        .front_budget_ms
        .unwrap_or(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS)
        .min(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS);
    let front_deadline =
        crate::services::codex_desktop_ipc::budget::Deadline::after_ms(front_budget_ms);
    let session_id = req.session_id.trim().to_string();
    if session_id.is_empty() {
        return bad_request();
    }
    let Some((device_id, device_name)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = session_id.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/turn/interrupt 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    match stop_current_turn(
        &st,
        &key,
        &session.id,
        &device_id,
        &device_name,
        req.turn_id.clone(),
        front_deadline,
    )
    .await
    {
        // 前置失败（阀值缺失 409 / 能力门 503 / 暂停写失败 500）：零帧，响应已就绪
        Err(ready) => ready,
        Ok(StopTurnOutcome::NotSent {
            reason,
            envelope_id,
        }) => {
            let ctx = ActionContext {
                action_id: "interrupt-none".to_string(),
                device_id,
                device_name,
                kind: ActionKind::Interrupt,
                key: key.clone(),
            };
            codex_audit(
                st.as_ref(),
                &ctx,
                AUDIT_INTERRUPT,
                &session.id,
                None,
                Some(&envelope_id),
                &format!("refused:{reason}"),
            );
            interrupt_not_sent_response(&reason)
        }
        Ok(StopTurnOutcome::Rejected { envelope_id }) => {
            let ctx = ActionContext {
                action_id: "interrupt-none".to_string(),
                device_id,
                device_name,
                kind: ActionKind::Interrupt,
                key: key.clone(),
            };
            // Desktop 明确拒绝（resultType=error 回执）——服务本身可用，语义是
            // 状态冲突而非「服务不可用」，映射 409（复审 P2-3）。
            codex_audit(
                st.as_ref(),
                &ctx,
                AUDIT_INTERRUPT,
                &session.id,
                None,
                Some(&envelope_id),
                "rejected",
            );
            error_body(
                StatusCode::CONFLICT,
                "desktop_rejected",
                Some("CODEX_INTERRUPT_DESKTOP_REJECTED: 桌面 Codex 明确拒绝了本次打断指令，回合未被停止"),
            )
        }
        Ok(StopTurnOutcome::Accepted {
            interrupted_turn_id,
            envelope_id,
            expected_turn_id,
            queue_pause_set,
        }) => {
            // 触发一次结算：等回合终态帧到达时由调度循环释放屏障（**队列接力仍被
            // 暂停挡住**——恢复只经用户显式「继续发送」，/codex/queue/resume）。
            st.codex_control.notify_settled(&key);
            let ctx = ActionContext {
                action_id: format!(
                    "interrupt-{}",
                    interrupted_turn_id.as_deref().unwrap_or("none")
                ),
                device_id,
                device_name,
                kind: ActionKind::Interrupt,
                key: key.clone(),
            };
            codex_audit(
                st.as_ref(),
                &ctx,
                AUDIT_INTERRUPT,
                &session.id,
                None,
                Some(&envelope_id),
                if interrupted_turn_id.is_some() {
                    "interrupted"
                } else {
                    "no_active_turn"
                },
            );
            json_no_store(
                StatusCode::OK,
                serde_json::json!({
                    "ok": true,
                    "uncertain": false,
                    "interruptedTurnId": interrupted_turn_id,
                    "expectedTurnId": expected_turn_id,
                    "queuePaused": queue_pause_set,
                }),
            )
        }
        Ok(StopTurnOutcome::Unknown {
            reason,
            envelope_id,
            expected_turn_id,
            queue_pause_set,
        }) => {
            // 帧已交 writer（超时/断连发生在写后）——打断**可能已生效**。如实回
            // 200 + uncertain:true，绝不报「停止失败」诱导用户立刻重按（重按 =
            // 双发破坏性指令）。结算照常触发：若打断实际已生效，终态证据会释放屏障。
            st.codex_control.notify_settled(&key);
            let ctx = ActionContext {
                action_id: "interrupt-none".to_string(),
                device_id,
                device_name,
                kind: ActionKind::Interrupt,
                key: key.clone(),
            };
            codex_audit(
                st.as_ref(),
                &ctx,
                AUDIT_INTERRUPT,
                &session.id,
                None,
                Some(&envelope_id),
                &format!("uncertain:{reason}"),
            );
            json_no_store(
                StatusCode::OK,
                serde_json::json!({
                    "ok": true,
                    "uncertain": true,
                    "interruptedTurnId": serde_json::Value::Null,
                    "expectedTurnId": expected_turn_id,
                    "reasonCode": reason,
                    "queuePaused": queue_pause_set,
                }),
            )
        }
    }
}

// ============================================================
// 端点：POST /codex/queue/prioritize（T06：已入队消息的显式优先处理闭环）
// ============================================================

/// POST /codex/queue/prioritize 请求体（camelCase；deny_unknown_fields 与同族端点
/// 同纪律）。**不接受正文副本**：优先处理派发的是既有 queued 行（原 actionId / 原
/// wire 文本 / 原 client_message_id）——带 `text`/`content` 等任何多余键 → 400
///（「把正文再 POST 一次」是另一条新消息，不是优先处理）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueuePrioritizeReq {
    pub session_id: String,
    pub action_id: String,
    /// 定向停止安全阀（active 确认停止时使用；缺省取在册原生屏障 / 镜像活动回合，
    /// 与 /codex/turn/interrupt 同纪律——三路取不到 → 409 拒发，零帧）。
    #[serde(default)]
    pub expected_turn_id: Option<String>,
    /// active（有运行中回合）时必须 true 才会停止当前回合；false + Busy → 明确
    /// 零停止零越队（not_sent）。
    pub confirm_interrupt: bool,
    /// T06：前台剩余预算（毫秒；省略 = 30_000，上限夹到 30_000）。停止、终态确认
    /// 等待与越队派发**共用同一绝对截止**，不逐层重置；到期只回明确结果，不在后台
    /// 偷偷重发。
    #[serde(default, rename = "frontBudgetMs")]
    pub front_budget_ms: Option<u64>,
}

/// 优先处理响应判别联合（`result` 判别；臂名 snake_case，字段 camelCase）。
/// 携带**原 actionId** 与既有动作回执；旧队列不受影响（本端点不解除任何暂停行）。
#[derive(Debug, Serialize)]
#[serde(
    tag = "result",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PrioritizeResp {
    /// 越队派发已受理（一次性授权恰好消费一次、恰好一次写出；队列保持暂停）。
    Prioritized {
        action_id: String,
        receipt: ActionReceipt,
    },
    /// 原动作已离开队列（并发派发赢者/已终态）——返回账本实际状态，零新写入。
    AlreadyDispatched {
        action_id: String,
        action: CodexActionView,
    },
    /// 明确零投递。`queued` = 动作是否仍在队列（true = 入口保留可重试；false =
    /// 已终态/已离开队列）。`receipt` = 已落账的最终回执（未落账为 null）。
    NotSent {
        action_id: String,
        reason_code: String,
        receipt: Option<ActionReceipt>,
        queued: bool,
    },
    /// 结果未知（停止不确定 / 前台预算到期）——未派发、不后台重试，稍后核对。
    Unknown {
        action_id: String,
        reason_code: String,
    },
}

/// T06：停止受理后的回合终态确认——**同一前台预算**内有界轮询新鲜门（`Clear` =
/// 被停回合已终态且无 pending）；到期返回 false（不派发、不后台重试）。轮询间隔
/// 100ms；每次判定都吃剩余预算（不重置）。
async fn wait_turn_terminal(
    hub: &CodexControlHub,
    key: &ThreadKey,
    front: crate::services::codex_desktop_ipc::budget::Deadline,
) -> bool {
    loop {
        if front.is_expired() {
            return false;
        }
        let gate_fut = fresh_send_gate(hub, key);
        match front.within(gate_fut).await {
            Ok(SendGate::Clear) => {
                let projection = hub.adapter.projection(key).await;
                let reconcile = settle_turn_evidence(hub, key, &projection);
                return front.within(reconcile).await.is_ok();
            }
            Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            Err(_) => return false,
        }
    }
}

/// 已离开 queued 的动作 → 响应（already_dispatched 携账本实际状态；cancelled =
/// 明确零投递、墓碑不复活）。并发赢者/重放的统一出口，零新写入。
fn prioritize_left_queue_response(action_id: &str, view: CodexActionView) -> Response {
    if view.state == ActionState::Cancelled {
        return json_no_store(
            StatusCode::OK,
            serde_json::to_value(&PrioritizeResp::NotSent {
                action_id: action_id.to_string(),
                reason_code: "action_cancelled".to_string(),
                receipt: view.receipt.clone(),
                queued: false,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        );
    }
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&PrioritizeResp::AlreadyDispatched {
            action_id: action_id.to_string(),
            action: view,
        })
        .unwrap_or_else(|_| serde_json::json!({})),
    )
}

/// C02：停止尝试的「未知」响应（重放与首次编排共用同一语义；`reason` 为机器词）。
fn prioritize_stop_unknown_response(action_id: &str, reason: &str) -> Response {
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&PrioritizeResp::Unknown {
            action_id: action_id.to_string(),
            reason_code: reason.to_string(),
        })
        .unwrap_or_else(|_| serde_json::json!({})),
    )
}

/// C02：停止尝试的「未发送」响应（`queued` = 动作是否留队）。
fn prioritize_not_sent_response(action_id: &str, reason: &str, queued: bool) -> Response {
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&PrioritizeResp::NotSent {
            action_id: action_id.to_string(),
            reason_code: reason.to_string(),
            receipt: None,
            queued,
        })
        .unwrap_or_else(|_| serde_json::json!({})),
    )
}

/// C02：释放停止尝试的占有（**仅首写前明确零投递 / Desktop 权威零打断效果 /
/// 停止完全收敛**的出口调用——「帧可能已生效或是否已写出未知」一律不调用，
/// 让占有留守、重放拒发新停止）。释放失败不回滚调用方的原响应：占有留守是
/// 保守方向（重放按 Unknown 拒发新停止），只记 error 留痕。
async fn release_stop_claim(store: &ActionStore, action_id: &str) {
    match store.release_stop_attempt(action_id) {
        Ok(true) => {}
        Ok(false) => {
            // 占有已不在（并发撤回赢者删行等）——无需释放，留 info 痕。
            log::info!("codex/queue/prioritize 停止占有已不在（无需释放）: action={action_id}");
        }
        Err(e) => {
            log::error!(
                "codex/queue/prioritize 停止占有释放失败（占有留守，重放按 unknown 拒发新停止）: {e}"
            );
        }
    }
}

/// POST /m/api/v1/codex/queue/prioritize：**已入队消息的显式优先处理闭环**——
/// 用户对具体 queued 消息点「先处理这条」：运行中先确认停止当前回合（真实定向
/// 停止 + 终态确认），该消息经**绑定原 action 的一次性越队授权**恰好发送一帧；
/// 旧队列保持暂停（本端点不解除任何暂停行、不自动恢复接力——恢复仍只经显式
/// resume）；不重排 FIFO（Q1/Q2 的 seq/位置原样）。
///
/// 契约与红线：
/// - **不接受正文副本**（deny_unknown_fields 拒 `text` 等）——「再 POST 一次正文」
///   是新消息，不是优先处理；越队派发的是原 queued 行（原 wire 文本、原
///   client_message_id、原 actionId）；
/// - 属主隔离（retract 同款 404 语义）：设备归属**与动作线程归属**（R01：请求
///   sessionId 解析出的线程 ≠ 动作行的线程 → 404，先于一切 mutation）都核对；
///   非 queued 入口即回实际状态（already_dispatched / cancelled 不复活），零新写入；
/// - 无确认（`confirmInterrupt=false`）+ 运行中 → 明确零停止零越队；
/// - R04：空闲但队列**未暂停** → 明确零越队（`queue_not_paused`）——越队只允许
///   「空闲且已暂停」的单次越队，未暂停的 Clear 保持原 FIFO（不建授权、零帧）；
/// - 停止 Unknown → `unknown`，不派 Q3、不自动重试；**同 actionId 重放不再发
///   停止帧**（R05/C02：停止尝试在**首次可能写出之前**持久占有（claim）、未知
///   结果升级记录、首写前明确零投递才释放——取消/崩溃窗同样被占有覆盖，重放读
///   到占有/未知一律按既有 unknown 语义返回，重启不丢；占有读/写失败保守拒发
///   新停止）；目标换代（expectedTurnId
///   不匹配 → Desktop 静默 no-op）→ 新鲜门仍 Busy → 零派发；原生 pending / 未解
///   Unknown 屏障 / 过预算 / 设备失效 → 全部零越队帧；
/// - 停止帧与消息帧**分别计数**：成功闭环 = 一次 interrupt + 一次 Q3 start-turn。
pub async fn queue_prioritize(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<QueuePrioritizeReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let session_id = req.session_id.trim().to_string();
    let action_id = req.action_id.trim().to_string();
    if session_id.is_empty() || action_id.is_empty() {
        return bad_request();
    }
    // R13 同纪律：前台总预算自**本服务端接收**起算；停止、终态等待与派发共用。
    let front_budget_ms = req
        .front_budget_ms
        .unwrap_or(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS)
        .min(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS);
    let front = crate::services::codex_desktop_ipc::budget::Deadline::after_ms(front_budget_ms);
    let Some((device_id, device_name)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = session_id.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/queue/prioritize 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    let store = st.codex_control.router.store();
    // 属主隔离 + 入口状态判别（retract 同款 404 语义）：非 queued 一律不重复派发
    //（并发 scheduler 取走 → already_dispatched 回实际状态；撤回赢者 → 不复活）。
    let view = match store.get_for_device(&action_id, &device_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue/prioritize 查询失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    let Some(view) = view else {
        return error_body(StatusCode::NOT_FOUND, "no_action", None);
    };
    if view.state != ActionState::Queued {
        return prioritize_left_queue_response(&action_id, view);
    }
    // R01 线程归属硬门：动作行的完整线程必须与请求会话解析出的线程一致——在
    // 任何停止、暂停、授权或动作 mutation 之前核对。不一致（含读取失败 → 空串
    // 保守拒绝）= 属主隔离同款 404（不泄露存在性），零帧、零授权、零账本写入。
    let action_thread = store
        .action_thread_and_cmid(&action_id)
        .map(|(t, _)| t)
        .unwrap_or_default();
    if action_thread != key.thread_id {
        return error_body(StatusCode::NOT_FOUND, "no_action", None);
    }
    let hub = &st.codex_control;
    // 全程线程派发锁（跨「门判定 + 定向停止 + 终态等待 + 越队派发」）：并发的第二
    // 次优先处理在此让路——不重复发停止帧（同一逻辑动作的重放至多一次 interrupt）、
    // 不并发越队（授权 CAS 仍兜底恰好一次写出）。与 send/flush 共用同一线程锁。
    let thread_lock = match hub.router.thread_dispatch_lock(&key).try_lock_owned() {
        Ok(g) => g,
        Err(_) => {
            // 在飞派发/编排占锁：授权未消费、零写入、零停止，按仍在队回应
            return json_no_store(
                StatusCode::OK,
                serde_json::to_value(&PrioritizeResp::NotSent {
                    action_id,
                    reason_code: "dispatch_in_flight".to_string(),
                    receipt: None,
                    queued: true,
                })
                .unwrap_or_else(|_| serde_json::json!({})),
            );
        }
    };
    // C02 幂等归属（先读后动，R05 语义保留并升级为占有状态机）：本动作已有活跃
    // 占有或未知结果记录 → 重放不再发停止帧（SQLite 持久，与动作行同生命周期、
    // 重启不丢），直接按既有未知语义返回。零帧、零 mutation。**读失败保守拒绝**：
    // 无法排除「已有在飞未知停止尝试」→ 按 Unknown 返回，不发新停止帧。
    let phase = match store.stop_attempt_phase(&action_id) {
        Ok(p) => p,
        Err(e) => {
            log::error!("codex/queue/prioritize 停止尝试账本读取失败（保守拒发新停止）: {e}");
            return prioritize_stop_unknown_response(&action_id, "stop_state_unavailable");
        }
    };
    match phase {
        // 未占有（从未尝试 / 上一次首写前零投递已释放）→ 允许占有并发起新停止
        StopAttemptPhase::Fresh | StopAttemptPhase::Released => {}
        // 已占有但结果未知（取消/崩溃窗）→ 不发新停止帧，按既有 Unknown 语义返回
        StopAttemptPhase::Claimed => {
            return prioritize_stop_unknown_response(&action_id, "stop_unknown");
        }
        // 已记录未知结果（R05 语义）→ 返回同一原因，零帧
        StopAttemptPhase::Unknown(reason) => {
            return prioritize_stop_unknown_response(&action_id, &reason);
        }
    }
    // 新鲜门分类（同一前台预算）：决定「直接越队」还是「先停止当前回合」。
    let gate = match front.within(fresh_send_gate(hub, &key)).await {
        Ok(g) => g,
        Err(_) => SendGate::StateUnknown,
    };
    match gate {
        // 原生 pending 在场：授权不穿透（与越队/派发路径同源守卫）
        SendGate::NativePending => {
            return json_no_store(
                StatusCode::OK,
                serde_json::to_value(&PrioritizeResp::NotSent {
                    action_id,
                    reason_code: crate::services::codex_control::NATIVE_PENDING_REASON.to_string(),
                    receipt: None,
                    queued: true,
                })
                .unwrap_or_else(|_| serde_json::json!({})),
            );
        }
        // 状态无法确认：按未确认处理（保守零帧，与写出门同口径）
        SendGate::StateUnknown => {
            return json_no_store(
                StatusCode::OK,
                serde_json::to_value(&PrioritizeResp::NotSent {
                    action_id,
                    reason_code: "state_unknown".to_string(),
                    receipt: None,
                    queued: true,
                })
                .unwrap_or_else(|_| serde_json::json!({})),
            );
        }
        // 无活动回合（空闲）：插队**直接放行**（2026-10-31 用户裁决：「插队就是
        // 停止进程之后，直接把这个消息打出去」——空闲时没有进程要停，用户的显式
        // 单条意图不依赖暂停状态）。
        //
        // 旧规则「空闲且队列已暂停才允许越队」（R04）与后来的「陈旧暂停自动清理」
        // 构成死结：暂停被清 → 队列未暂停 → 插队被 `queue_not_paused` 拒绝，而同一
        // 时刻屏障又挡住自动接力，于是插队永久不可用、队列也发不出去（实机现场）。
        // FIFO 与安全仍有别的守卫兜底：并发在飞派发由线程锁挡（dispatch_in_flight）、
        // 未解屏障由 `prioritize_queued` 的 `barrier_outstanding` 判定、原生 pending
        // 由上面的 NativePending 臂挡、停止结果未知由 stop_attempt 状态机挡——
        // 这些门一律不动。
        SendGate::Clear => {}
        SendGate::Busy => {
            if !req.confirm_interrupt {
                // 无确认：零停止、零越队（Q3 原样留队）
                return json_no_store(
                    StatusCode::OK,
                    serde_json::to_value(&PrioritizeResp::NotSent {
                        action_id,
                        reason_code: "interrupt_confirmation_required".to_string(),
                        receipt: None,
                        queued: true,
                    })
                    .unwrap_or_else(|_| serde_json::json!({})),
                );
            }
            // C02：**首次可能写出之前**持久占有停止尝试（CAS）——writer 已接收/
            // 写出 interrupt 后 handler 被取消或服务退出时，事后记录（Unknown/超时
            // 臂）永远执行不到；先占有把该盲窗闭合：重放读到占有即不再发停止帧。
            // **占有失败（写失败/行不在账/状态冲突）不得继续向 Desktop 发新停止**：
            // 本次确定零投递（占有在停止编排之前，任何帧都不可能已写出），按
            // NotSent 拒绝并留队，动作可重试（DB 恢复后占有成功才放行编排）。
            match store.claim_stop_attempt(&action_id) {
                Ok(true) => {}
                Ok(false) => {
                    return prioritize_not_sent_response(
                        &action_id,
                        "stop_claim_unavailable",
                        true,
                    );
                }
                Err(e) => {
                    log::error!("codex/queue/prioritize 停止尝试占有失败（保守拒发新停止）: {e}");
                    return prioritize_not_sent_response(
                        &action_id,
                        "stop_claim_unavailable",
                        true,
                    );
                }
            }
            // 真实定向停止（复用打断编排核心：阀值/能力门/前置暂停/有界打断/暂停收尾）
            match stop_current_turn(
                &st,
                &key,
                &session.id,
                &device_id,
                &device_name,
                req.expected_turn_id.clone(),
                front,
            )
            .await
            {
                // 前置失败（阀值缺失 409 / 能力门 503 / 暂停写失败 500）：零帧零越队。
                // C02：首写前明确零投递 → 释放占有（同 action 显式重试可再次编排）。
                Err(ready) => {
                    release_stop_claim(store, &action_id).await;
                    return ready;
                }
                Ok(StopTurnOutcome::NotSent {
                    reason,
                    envelope_id,
                }) => {
                    // C02：写尝试未开始（writer 短命许可/预算/能力/阀值复核拒绝）=
                    // 首写前明确零投递 → 释放占有。
                    release_stop_claim(store, &action_id).await;
                    let ctx = ActionContext {
                        action_id: "interrupt-none".to_string(),
                        device_id,
                        device_name,
                        kind: ActionKind::Interrupt,
                        key: key.clone(),
                    };
                    codex_audit(
                        st.as_ref(),
                        &ctx,
                        AUDIT_INTERRUPT,
                        &session.id,
                        None,
                        Some(&envelope_id),
                        &format!("refused:{reason}"),
                    );
                    return json_no_store(
                        StatusCode::OK,
                        serde_json::to_value(&PrioritizeResp::NotSent {
                            action_id,
                            reason_code: format!("stop_{reason}"),
                            receipt: None,
                            queued: true,
                        })
                        .unwrap_or_else(|_| serde_json::json!({})),
                    );
                }
                Ok(StopTurnOutcome::Rejected { envelope_id }) => {
                    // C02：Desktop 明确拒绝 = 该帧已被处理且未打断任何回合（权威
                    // 零打断效果）→ 释放占有（重试可再次编排；重发不构成双停止）。
                    release_stop_claim(store, &action_id).await;
                    let ctx = ActionContext {
                        action_id: "interrupt-none".to_string(),
                        device_id,
                        device_name,
                        kind: ActionKind::Interrupt,
                        key: key.clone(),
                    };
                    codex_audit(
                        st.as_ref(),
                        &ctx,
                        AUDIT_INTERRUPT,
                        &session.id,
                        None,
                        Some(&envelope_id),
                        "rejected",
                    );
                    return json_no_store(
                        StatusCode::OK,
                        serde_json::to_value(&PrioritizeResp::NotSent {
                            action_id,
                            reason_code: "stop_desktop_rejected".to_string(),
                            receipt: None,
                            queued: true,
                        })
                        .unwrap_or_else(|_| serde_json::json!({})),
                    );
                }
                Ok(StopTurnOutcome::Unknown { reason, .. }) => {
                    // R05/C02：停止结果未知（帧可能已生效）→ 把占有的结果升级为
                    // 未知原因（重放先读该记录，不再发第二帧）。升级失败不改本次
                    // 响应（与基线一致），只丢失具体原因——占有留守，重放按
                    // stop_unknown 同样拒发新停止（保守方向不变）。
                    let reason_code = format!("stop_{reason}");
                    match store.resolve_stop_attempt_unknown(&action_id, &reason_code) {
                        Ok(true) => {}
                        Ok(false) => {
                            log::warn!("codex/queue/prioritize 停止占有升级落空（占有已不在，重放仍拒发新停止）");
                        }
                        Err(e) => {
                            log::error!("codex/queue/prioritize 停止未知结果记录失败（占有留守，重放按 stop_unknown 拒发新停止）: {e}");
                        }
                    }
                    // 停止不确定：不派 Q3、不自动重试（暂停已保留，旧队列不解冻）
                    return prioritize_stop_unknown_response(&action_id, &reason_code);
                }
                Ok(StopTurnOutcome::Accepted {
                    interrupted_turn_id,
                    ..
                }) => {
                    // 终态确认（同一预算内有界等待）。Desktop 明确说「无可打断回合」
                    // （interruptedTurnId=null）且新鲜门仍 Busy = 目标换代/停止未生效
                    // → 立即零派发（不空耗预算、不误伤新回合）；Clear = 回合确已结束
                    // → 直接越队。
                    if interrupted_turn_id.is_none() {
                        // P8/P10（2026-10-31 用户实机）：Desktop 回 `interruptedTurnId:null`
                        // 有两种含义——(a) 目标换代/无可打断回合（真 no-op），
                        // (b) **停止已受理但状态帧还在路上**（真机常态延迟）。旧实现
                        // 只做一次 `fresh_send_gate` 就判 (a)，于是「第一次点插队：
                        // 进程被打断但消息没发出去；等远端结束后再点一次才成功」。
                        //
                        // 修法分两段（用户 2026-10-31 追加要求：「兜底 3 秒之后发现
                        // 还是没发出去，再补一个」）：
                        //  · 第一段：专属短窗 3s（STOP_SETTLE_WINDOW_MS）——真机被打断
                        //    的回合通常 1–2 秒内转终态，绝大多数请求在这一段就成功；
                        //  · 第二段：仍在同一前台预算内的**追加宽限**
                        //    （STOP_SETTLE_RETRY_WINDOW_MS）——覆盖「桌面停得更慢」
                        //    的长尾，避免用户必须再点一次。
                        // 两段都按剩余预算封顶（S3：子步骤不重置预算），因此真换代
                        // （永不转 Clear）最多耗 3s + 4s，不会吃光整条 30 秒预算。
                        // C02：Desktop 权威判定「无可打断回合」= 该帧已被处理且未
                        // 打断任何回合（零打断效果）→ 释放占有。
                        release_stop_claim(store, &action_id).await;
                        let mut settled = wait_turn_terminal(
                            hub,
                            &key,
                            settle_window(front, STOP_SETTLE_WINDOW_MS),
                        )
                        .await;
                        if !settled {
                            log::info!(
                                "codex/queue/prioritize: 停止后 {STOP_SETTLE_WINDOW_MS}ms 内未见终态，追加宽限 {STOP_SETTLE_RETRY_WINDOW_MS}ms（action={action_id}）"
                            );
                            settled = wait_turn_terminal(
                                hub,
                                &key,
                                settle_window(front, STOP_SETTLE_RETRY_WINDOW_MS),
                            )
                            .await;
                        }
                        if !settled {
                            return json_no_store(
                                StatusCode::OK,
                                serde_json::to_value(&PrioritizeResp::NotSent {
                                    action_id,
                                    reason_code: "stop_not_effective".to_string(),
                                    receipt: None,
                                    queued: true,
                                })
                                .unwrap_or_else(|_| serde_json::json!({})),
                            );
                        }
                    } else if !wait_turn_terminal(hub, &key, front).await {
                        // 受理但预算内未见终态：如实「待确认」——不派发、不后台重试。
                        // R05/C02：同样是「停止结果未知」，把占有的结果升级为未知
                        // 原因（重放不再发停止帧）；升级失败占有留守（保守不变）。
                        match store.resolve_stop_attempt_unknown(
                            &action_id,
                            crate::services::codex_control::FRONT_BUDGET_ELAPSED_REASON,
                        ) {
                            Ok(true) => {}
                            Ok(false) => {
                                log::warn!("codex/queue/prioritize 停止占有升级落空（占有已不在，重放仍拒发新停止）");
                            }
                            Err(e) => {
                                log::error!(
                                    "codex/queue/prioritize 停止未知结果记录失败（占有留守，重放按 stop_unknown 拒发新停止）: {e}"
                                );
                            }
                        }
                        return json_no_store(
                            StatusCode::OK,
                            serde_json::to_value(&PrioritizeResp::Unknown {
                                action_id,
                                reason_code:
                                    crate::services::codex_control::FRONT_BUDGET_ELAPSED_REASON
                                        .to_string(),
                            })
                            .unwrap_or_else(|_| serde_json::json!({})),
                        );
                    } else {
                        // C02：受理且预算内确认终态——停止尝试已完全收敛（不再有
                        // 「未知」需要防重放），释放占有；随后越队派发由授权 CAS
                        // 与既有语义把关。
                        release_stop_claim(store, &action_id).await;
                    }
                }
            }
        }
    }
    // paused-idle 和 Desktop 回报无活动回合的分支也须结算旧屏障；只认已有
    // 精确 Turn 终态证据，不将 idle 本身当成投递确认。
    let projection = hub.adapter.projection(&key).await;
    if front
        .within(settle_turn_evidence(hub, &key, &projection))
        .await
        .is_err()
    {
        return prioritize_not_sent_response(&action_id, "front_budget_elapsed", true);
    }
    // 越队派发既有 queued 行（授权绑定原 action、写出前 CAS 恰好消费一次；
    // 旧队列保持暂停，resume 仍是唯一恢复出口）。全程锁延续传入（不可重入）。
    match hub
        .router
        .prioritize_queued(&action_id, &key, front, thread_lock)
        .await
    {
        PrioritizeOutcome::Completed(receipt) => match receipt.delivery {
            Delivery::Accepted => json_no_store(
                StatusCode::OK,
                serde_json::to_value(&PrioritizeResp::Prioritized { action_id, receipt })
                    .unwrap_or_else(|_| serde_json::json!({})),
            ),
            Delivery::UnknownDelivery => json_no_store(
                StatusCode::OK,
                serde_json::to_value(&PrioritizeResp::Unknown {
                    action_id,
                    reason_code: receipt
                        .reason_code
                        .unwrap_or_else(|| "unknown_delivery".to_string()),
                })
                .unwrap_or_else(|_| serde_json::json!({})),
            ),
            // 明确零投递/明确拒绝已落账：原样回执（动作已终态，queued=false）
            Delivery::NotSent | Delivery::Rejected => json_no_store(
                StatusCode::OK,
                serde_json::to_value(&PrioritizeResp::NotSent {
                    reason_code: receipt
                        .reason_code
                        .clone()
                        .unwrap_or_else(|| "not_sent".to_string()),
                    action_id,
                    receipt: Some(receipt),
                    queued: false,
                })
                .unwrap_or_else(|_| serde_json::json!({})),
            ),
        },
        // 授权未消费、零落账：动作仍在队（入口保留，可重试）
        PrioritizeOutcome::StillQueued(reason) => json_no_store(
            StatusCode::OK,
            serde_json::to_value(&PrioritizeResp::NotSent {
                action_id,
                reason_code: reason.to_string(),
                receipt: None,
                queued: true,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
        PrioritizeOutcome::LeftQueue => match store.get_for_device(&action_id, &device_id) {
            Ok(Some(v)) => prioritize_left_queue_response(&action_id, v),
            Ok(None) => error_body(StatusCode::NOT_FOUND, "no_action", None),
            Err(e) => {
                log::error!("codex/queue/prioritize 复读失败: {e}");
                json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                )
            }
        },
    }
}

// ============================================================
// 端点：POST /codex/pending-response
// ============================================================

/// POST /codex/pending-response 请求体。**deny_unknown_fields**：`ownerClientId` /
/// `targetClientId` / raw method / permissions 对象等任何多余键 → 400（HTTP 层不出现
/// IPC owner 参数——服务端按 Owner Registry 自行解析）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingResponseReq {
    pub session_id: String,
    pub action_id: String,
    pub request_key: String,
    pub context_token: String,
    /// `"approve" | "reject" | "answer"`
    pub action: String,
    /// action=answer 必填：按原问题 ID 映射的标签数组（isSecret 答案只入 hash）
    #[serde(default)]
    pub answers: Option<BTreeMap<String, Vec<String>>>,
    /// T04：客户端传入的**前台剩余预算**（毫秒；省略 = 30_000，上限夹到 30_000，
    /// 非负整数；服务端不信远端绝对时钟，只认本机起算的相对量）。
    #[serde(default, rename = "frontBudgetMs")]
    pub front_budget_ms: Option<u64>,
}

/// 回答限额（计划 Task 8 端点表）：每题 ≤10_000 字符、≤32 题、每题 ≤32 答案、总正文 ≤64 KiB。
const ANSWER_MAX_QUESTIONS: usize = 32;
const ANSWER_MAX_ANSWERS_PER_Q: usize = 32;
const ANSWER_MAX_CHARS_PER_ANSWER: usize = 10_000;
const ANSWER_MAX_TOTAL_BYTES: usize = 64 * 1024;

/// POST /m/api/v1/codex/pending-response：经 router.respond（内部做 lease/token/pending/
/// 能力最后复核）→ 200 ActionReceipt。
/// - 带 owner/target/raw/permissions → 400（deny_unknown_fields）；
/// - 过期 token / 跨 thread requestKey → 409 且 IPC 写 0（adapter 写前复核拒绝）；
/// - unknown_delivery 是 200 的明确回执；能力关 → 503。
pub async fn pending_response(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<PendingResponseReq>, JsonRejection>,
) -> Response {
    // deny_unknown_fields / 缺字段 / 非 JSON → 统一 400 malformed
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    // T04/R13：前台总预算自**本服务端接收**起算（客户端传剩余值）；后续的会话扫描、
    // 投影、前置暂停等都消耗同一预算——不再在完成前置工作后才起算整段 30s。
    let front_budget_ms = req
        .front_budget_ms
        .unwrap_or(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS)
        .min(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS);
    let front_deadline =
        crate::services::codex_desktop_ipc::budget::Deadline::after_ms(front_budget_ms);
    let hub = &st.codex_control;
    // #172 解耦：审批只要求**读链可用**（能在投影里看到 pending）——写决策的类别能力
    // （command / permissions / blocking_input）由 adapter 按当前 profile 逐项门控，
    // 不在端点入口用 `send` 连坐。disabled hub / 读链未取证 → 503 capability_unavailable。
    if !hub.state_available() {
        return capability_unavailable();
    }
    let sid = req.session_id.trim().to_string();
    let action_id = req.action_id.trim().to_string();
    let request_key = req.request_key.trim().to_string();
    let context_token = req.context_token.trim().to_string();
    if sid.is_empty() || action_id.is_empty() || request_key.is_empty() || context_token.is_empty()
    {
        return bad_request();
    }
    let Some((device_id, device_name)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/pending-response 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    // 动作词 + 答案校验（限额/空答案；qid 归属由 adapter 对当前请求精确复核）
    // T05：cancel = 拒绝并终止本轮（与 reject 明确分开）。
    let (pending_action, audit_action) = match req.action.as_str() {
        "approve" => (PendingAction::Approve, AUDIT_APPROVE),
        "reject" => (PendingAction::Reject, AUDIT_REJECT),
        "cancel" => (PendingAction::Cancel, AUDIT_REJECT),
        "answer" => {
            let Some(answers) = req.answers.as_ref() else {
                return bad_request();
            };
            if let Err(code) = validate_answers(answers) {
                return error_body(StatusCode::BAD_REQUEST, code, None);
            }
            (PendingAction::Answer(answers.clone()), AUDIT_ANSWER)
        }
        _ => return bad_request(),
    };
    // 当前投影上的 pending（kind → 账本 ActionKind；不在场 = 跨 thread requestKey 或
    // 已消失——409 stale_context 且零写入）
    let projection = hub.adapter.projection(&key).await;
    // N02：派发基线的镜像版本——完成条目必须比它更新，才可能属于本动作。
    let Some(pending) = projection
        .pending
        .iter()
        .find(|p| p.request_key == request_key)
    else {
        return error_body(StatusCode::CONFLICT, "stale_context", None);
    };
    let kind = match pending.kind {
        PendingKind::CommandApproval => ActionKind::CommandApproval,
        // #172/T7：文件变更审批与命令审批同属 L3（账本动作类别共用）
        PendingKind::FileChangeApproval => ActionKind::FileChangeApproval,
        PendingKind::PermissionsApproval => ActionKind::PermissionsApproval,
        PendingKind::UserInput => ActionKind::UserInput,
        PendingKind::Unsupported => return capability_unavailable(),
    };
    // 动作与 pending 类别相容（approve/reject 不可用于 user_input，反之亦然）
    let compatible = matches!(
        (&pending_action, pending.kind),
        (PendingAction::Approve, PendingKind::CommandApproval)
            | (PendingAction::Approve, PendingKind::FileChangeApproval)
            | (PendingAction::Approve, PendingKind::PermissionsApproval)
            | (PendingAction::Reject, PendingKind::CommandApproval)
            | (PendingAction::Reject, PendingKind::FileChangeApproval)
            | (PendingAction::Reject, PendingKind::PermissionsApproval)
            // T05：cancel（拒绝并终止本轮）只对命令/文件审批；权限/问答没有该出口。
            | (PendingAction::Cancel, PendingKind::CommandApproval)
            | (PendingAction::Cancel, PendingKind::FileChangeApproval)
            | (PendingAction::Answer(_), PendingKind::UserInput)
    );
    if !compatible {
        return error_body(StatusCode::CONFLICT, "action_mismatch", None);
    }
    // H01：入口捕获本动作的**效果目标**（后续派发期间换目标也不污染）。
    let effect_target = hub.capture_observed_target();
    let ctx = ActionContext {
        action_id,
        device_id,
        device_name,
        kind,
        key: key.clone(),
    };
    // C06（第三轮复审）：原生阻塞问答的“全局 revision 前进 + 全历史 requestId
    // completed”自动成功归因**暂不启用**——宿主重启复用整数 requestId 时，旧
    // completed 可能被误认本动作成功。契约匹配/操作可用性不受影响；待原 turn/item
    // 与本次答案的强关联接通后再恢复 ObservedSuccess 提升。
    // R07：暂停的创建在这里（同步、必须先于派发），**归属收尾放进独立任务**——
    // HTTP 等待者超时/断开不再拥有清理权：后台按真实结果（NotSent/Rejected 才撤
    // 本次 token 的暂停）收尾，避免"等待者被取消 → 暂停遗留"。
    // H04：**派发前**复核前台预算——前置工作（扫描/投影/暂停）耗尽预算时，
    // 新动作必须零派发（不能靠后台 120s 预算继续批）。
    if front_deadline.is_expired() {
        // H04：同 ID 已有动作 → **幂等返回既有状态**（不改账本；Reserved 等 ACK
        // 时如实 Unknown）；不同载荷 → 409；全新动作 → 零投递回执且**不登记**。
        if let Some(row) = hub
            .router
            .store()
            .snapshot_for_device(&ctx.action_id, &ctx.device_id)
        {
            let payload = match &pending_action {
                PendingAction::Answer(answers) => {
                    crate::services::codex_control::ActionPayload::Answers(answers.clone())
                }
                PendingAction::Approve => {
                    crate::services::codex_control::ActionPayload::Text("approve".to_string())
                }
                PendingAction::Reject => {
                    crate::services::codex_control::ActionPayload::Text("reject".to_string())
                }
                PendingAction::Cancel => {
                    crate::services::codex_control::ActionPayload::Text("cancel".to_string())
                }
            };
            let expect_hash = crate::services::codex_control::payload_hash(
                kind,
                &key,
                Some(&request_key),
                Some(&context_token),
                &payload,
            );
            let same_payload = row.payload_hash == expect_hash
                && row.request_key.as_deref() == Some(request_key.as_str())
                && row.context_token.as_deref() == Some(context_token.as_str())
                && row.kind
                    == match kind {
                        ActionKind::CommandApproval => "command_approval",
                        ActionKind::FileChangeApproval => "file_change_approval",
                        ActionKind::PermissionsApproval => "permissions_approval",
                        ActionKind::UserInput => "user_input",
                        _ => "send",
                    };
            if !same_payload {
                return error_body(StatusCode::CONFLICT, "action_conflict", None);
            }
            let receipt = row.receipt.clone();
            return respond_receipt_response(receipt.unwrap_or_else(|| {
                crate::services::codex_control::router::unknown_receipt(&ctx.action_id)
            }));
        }
        let receipt = ActionReceipt {
            action_id: ctx.action_id.clone(),
            delivery: Delivery::NotSent,
            route: crate::services::codex_desktop_ipc::types::ControlRoute::None,
            turn_id: None,
            envelope_id: None,
            reason_code: Some(
                crate::services::codex_control::FRONT_BUDGET_ELAPSED_REASON.to_string(),
            ),
        };
        return respond_receipt_response(receipt);
    }
    // H06：暂停的创建、重发布、派发与**归属收尾**全部交给同一个独立任务——HTTP
    // 取消/断开不再可能停在"暂停已落库、任务尚未接管"的窗口里（旧代码在 spawn
    // 前 await 重发布，等待者取消即遗留孤儿暂停）。
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel::<Result<RouterOutcome, String>>();
    {
        let task_hub = hub.clone();
        let task_ctx = ctx.clone();
        let task_request = request_key.clone();
        let task_token = context_token.to_string();
        let task_action = pending_action.clone();
        let is_cancel =
            matches!(task_action, PendingAction::Cancel) && !front_deadline.is_expired();
        // H04：**派发资格**沿用前台剩余预算（到点即不再写出）；动作一旦开始写出，
        // 收尾由 `respond_bounded` 内部的独立结算任务按后台预算有界完成——两个
        // 概念不互相替换。原先这里新建 120s 预算会把过期动作重新放大。
        let dispatch_deadline = front_deadline;
        tokio::spawn(async move {
            let thread_key = task_ctx.key.clone();
            let action_id = task_ctx.action_id.clone();
            // H06：暂停必须在指令之前建立；失败 → 零帧并如实回 5xx（由等待者映射）。
            // P3：cancel 暂停行绑定镜像当前活动回合（取不到 = NULL，保守保留）。
            let task_pause_token = if is_cancel {
                let cancel_turn = task_hub.adapter.mirror_active_turn_id(&thread_key).await;
                match task_hub.prepare_turn_stop_pause(
                    &thread_key,
                    "cancel_pending",
                    cancel_turn.as_deref(),
                ) {
                    Ok((token, inserted)) => {
                        if inserted {
                            task_hub
                                .adapter
                                .republish_thread_projection(&thread_key)
                                .await;
                        }
                        Some(token)
                    }
                    Err(e) => {
                        log::error!(
                            "cancel 前置暂停写入失败（放弃本次终止，未向桌面发送任何指令）: {e}"
                        );
                        let _ = outcome_tx.send(Err(e.to_string()));
                        return;
                    }
                }
            } else {
                None
            };
            // H06：暂停归属跟随**真实最终结果**（router 结算任务回调）——前台
            // Unknown 不销毁归属；明确未发生才撤本次 token，受理/未知保留。
            let pause_hook = task_pause_token.clone().map(|token| {
                let hook_hub = task_hub.clone();
                let hook_key = thread_key.clone();
                let hook: crate::services::codex_control::router::SettleHook =
                    std::sync::Arc::new(move |delivery| {
                        if !matches!(delivery, Delivery::NotSent | Delivery::Rejected) {
                            return;
                        }
                        match hook_hub
                            .router
                            .store()
                            .clear_queue_pause_owned(&hook_key, &token)
                        {
                            Ok(removed) if removed => {
                                let republish_hub = hook_hub.clone();
                                let republish_key = hook_key.clone();
                                tokio::spawn(async move {
                                    republish_hub
                                        .adapter
                                        .republish_thread_projection(&republish_key)
                                        .await;
                                });
                            }
                            Ok(_) => {}
                            Err(e) => log::error!("cancel 前置暂停撤回失败（保留保守暂停）: {e}"),
                        }
                    });
                hook
            });
            let outcome = task_hub
                .router
                .respond_bounded(
                    task_ctx,
                    task_request,
                    task_token,
                    task_action,
                    dispatch_deadline,
                    crate::services::codex_control::router::EffectSettlement {
                        target: effect_target,
                        on_settled: pause_hook,
                    },
                )
                .await;

            // N08：router 早退（front 到期/登记前拒绝/忙）不经过结算任务——本任务
            // 直接按本次 token 收尾；受理/未知保留，迟到最终结果再由 hook 收尾。
            if let (Some(token), RouterOutcome::Completed(receipt)) =
                (task_pause_token.as_deref(), &outcome)
            {
                if matches!(receipt.delivery, Delivery::NotSent | Delivery::Rejected) {
                    match task_hub
                        .router
                        .store()
                        .clear_queue_pause_owned(&thread_key, token)
                    {
                        Ok(true) => {
                            let republish_hub = task_hub.clone();
                            let republish_key = thread_key.clone();
                            tokio::spawn(async move {
                                republish_hub
                                    .adapter
                                    .republish_thread_projection(&republish_key)
                                    .await;
                            });
                        }
                        Ok(false) => {}
                        Err(e) => log::error!("cancel 前置暂停撤回失败（保留保守暂停）: {e}"),
                    }
                }
            }
            let _ = action_id;
            let _ = outcome_tx.send(Ok(outcome));
        });
    }
    // 前台只等**自己的剩余预算**：到期返回 Unknown（可能已写出），后台继续收尾。
    // oneshot 的 `Result<Result<_, _>, RecvError>` 再套一层预算 `Result`：
    // 最内层 = 任务结果；中间 Err(原因) = 暂停建立失败；通道断开/预算到期 = 未知。
    let router_outcome = match front_deadline.within(outcome_rx).await {
        Ok(Ok(Ok(outcome))) => outcome,
        Ok(Ok(Err(reason))) => {
            // H06：暂停建立失败 = 明确未向桌面发送任何指令（可安全重试）。
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({
                    "error": "internal",
                    "reason": format!(
                        "CODEX_CANCEL_PAUSE_WRITE_FAILED: 队列暂停保护写入失败，已放弃本次终止（未向桌面发送任何指令），请稍后重试 ({reason})"
                    ),
                }),
            );
        }
        Ok(Err(_)) | Err(_) => {
            log::info!(
                "cancel/respond 前台等待结束（后台继续收尾）action={}",
                ctx.action_id
            );
            RouterOutcome::Completed(crate::services::codex_control::router::unknown_receipt(
                &ctx.action_id,
            ))
        }
    };
    match router_outcome {
        RouterOutcome::Completed(mut receipt) => {
            // #172/T7：**受理 ≠ 生效**——decision 之后等 requestId 从新 snapshot 消失
            //（≤ `decision_confirm_ms`）；仍在场 → 如实标 `decision_unconfirmed`
            //（前端提示「未确认，可终止本轮」），绝不假装成功。
            if receipt.delivery == Delivery::Accepted {
                receipt = confirm_decision_gone(
                    hub,
                    &ctx.key,
                    &request_key,
                    &projection,
                    receipt,
                    front_deadline,
                )
                .await;
            }
            let result = audit_result_of_receipt(&receipt);
            codex_audit(
                &st,
                &ctx,
                audit_action,
                &session.id,
                Some(&request_key),
                receipt.envelope_id.as_deref(),
                &result,
            );
            respond_receipt_response(receipt)
        }
        // Reserved 重放（同 action 在途）：按冲突口径（客户端会以新 actionId 重试）
        RouterOutcome::Queued { .. } => error_body(StatusCode::CONFLICT, "action_conflict", None),
        // S2：UseFallback 变体已随兜底臂从 RouterOutcome 删除（审批从不降级——
        // 该不变量由枚举形状结构性保证，不再需要防御臂）。
    }
}

/// #172/T7「回执」：decision 后等该 request 从新 snapshot 消失，最多
/// `decision_confirm_ms`（默认 5s，测试可收窄到 0）。
///
/// 语义：`Accepted` 只代表 Desktop **受理**了决策帧；只有 pending 从快照里消失才是
/// 「生效」。超时仍未见消失 → 回执加 `decision_unconfirmed`（前端提示「未确认，可
/// 终止本轮」），绝不把受理当成功。
async fn confirm_decision_gone(
    hub: &CodexControlHub,
    key: &ThreadKey,
    request_key: &str,
    before: &ThreadProjection,
    mut receipt: ActionReceipt,
    front: crate::services::codex_desktop_ipc::budget::Deadline,
) -> ActionReceipt {
    let total = hub.decision_confirm_ms();
    if total == 0 {
        return receipt;
    }
    // R13：确认窗**不重置**前台预算——只等剩余时间（到点立即如实「未确认」）。
    let remaining = front
        .remaining()
        .unwrap_or_else(|| std::time::Duration::from_millis(total));
    let wait = remaining.min(std::time::Duration::from_millis(total));
    if wait.is_zero() {
        if receipt.reason_code.is_none() {
            receipt.reason_code = Some(
                crate::services::codex_desktop_ipc::reason_code::DECISION_UNCONFIRMED.to_string(),
            );
        }
        return receipt;
    }
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let projection = hub.adapter.projection(key).await;
        if decision_snapshot_confirms(before, &projection, request_key) {
            return receipt;
        }
        if tokio::time::Instant::now() >= deadline {
            if receipt.reason_code.is_none() {
                // #172/T9a：机器码单一出处（前端按 `errors.decisionUnconfirmed` 取文案）
                let code = crate::services::codex_desktop_ipc::reason_code::DECISION_UNCONFIRMED;
                log::info!(
                    "codex-ipc decision: 受理后 {total}ms 未见生效，reason={code} userMessage={}",
                    crate::services::codex_desktop_ipc::reason_code::message_of(code)
                        .unwrap_or("-")
                );
                receipt.reason_code = Some(code.to_string());
            }
            return receipt;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

fn decision_snapshot_confirms(
    before: &ThreadProjection,
    current: &ThreadProjection,
    request_key: &str,
) -> bool {
    !current.stale
        && current.owner_epoch == before.owner_epoch
        && current.owner_generation == before.owner_generation
        && current
            .revision
            .zip(before.revision)
            .is_some_and(|(now, old)| now > old)
        && !current.pending.iter().any(|p| p.request_key == request_key)
}

/// 回执 → HTTP：非 200 档（409/400/503）返回**错误体** `{"error": <机器码>}`（过期
/// 上下文/冲突是可判别的拒绝语义，不是投递回执）；其余（含 not_sent 与
/// unknown_delivery）都是 200 的明确 ActionReceipt 回执。
fn respond_receipt_response(receipt: ActionReceipt) -> Response {
    let reason = receipt.reason_code.clone().unwrap_or_default();
    let status = match reason.as_str() {
        "stale_context" | "action_conflict" => StatusCode::CONFLICT,
        "answers_mismatch" | "answer_not_option" | "invalid_id" => StatusCode::BAD_REQUEST,
        "capability_unavailable" | "unsupported" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::OK,
    };
    if status == StatusCode::OK {
        return json_no_store(
            status,
            serde_json::to_value(&receipt).unwrap_or_else(|_| serde_json::json!({})),
        );
    }
    error_body(status, &reason, None)
}

/// 回答限额校验：空答案 / 超题数 / 超答案数 / 超长 / 总正文超 64 KiB → Err(机器码)。
fn validate_answers(answers: &BTreeMap<String, Vec<String>>) -> Result<(), &'static str> {
    if answers.is_empty() {
        return Err("empty_answers");
    }
    if answers.len() > ANSWER_MAX_QUESTIONS {
        return Err("too_many_questions");
    }
    for (qid, entries) in answers {
        if qid.trim().is_empty() || entries.is_empty() {
            return Err("empty_answers");
        }
        if entries.len() > ANSWER_MAX_ANSWERS_PER_Q {
            return Err("too_many_answers");
        }
        for e in entries {
            if e.chars().count() > ANSWER_MAX_CHARS_PER_ANSWER {
                return Err("answer_too_long");
            }
        }
    }
    let total: usize = answers
        .iter()
        .map(|(q, v)| q.len() + v.iter().map(|s| s.len() + 2).sum::<usize>())
        .sum();
    if total > ANSWER_MAX_TOTAL_BYTES {
        return Err("answers_too_large");
    }
    Ok(())
}

/// 设备身份（cookie 已过 gate；此处提取 + 花名，防御 403）。
pub(crate) fn device_identity(st: &RemoteState, headers: &HeaderMap) -> Option<(String, String)> {
    let device_id = super::gate::extract_device(headers)?;
    let device_name = st
        .store
        .with(|c| super::pairing::device_name(c, &device_id));
    Some((device_id, device_name))
}

#[cfg(test)]
mod startup_merge_tests {
    //! 2026-10-20 复审 P2：SSE 首快照与订阅缓冲的合并语义（纯函数层，确定性覆盖
    //! 「快照与建流交界处发布最终态」「旧缓冲不得覆盖较新快照」「owner 换代」）。

    use super::{
        decision_snapshot_confirms, merge_startup_buffer, supersedes, turn_association_refusal,
    };
    use crate::services::codex_desktop_ipc::types::{
        CapabilitySet, ContentState, ControlAvailability, ProbeStatus, ThreadKey, ThreadProjection,
    };

    fn k(thread: &str) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: thread.into(),
        }
    }

    #[test]
    fn decision_confirmation_rejects_disconnect_and_old_snapshots() {
        let before = p("t", 1, 2, Some(10), false);
        assert!(decision_snapshot_confirms(
            &before,
            &p("t", 1, 2, Some(11), false),
            "r"
        ));
        for current in [
            p("t", 1, 2, Some(11), true),
            p("t", 1, 2, Some(10), false),
            p("t", 2, 2, Some(11), false),
            p("t", 1, 3, Some(11), false),
            p("t", 1, 2, None, false),
        ] {
            assert!(!decision_snapshot_confirms(&before, &current, "r"));
        }
    }

    fn p(thread: &str, epoch: u64, gen: u64, rev: Option<u64>, stale: bool) -> ThreadProjection {
        ThreadProjection {
            thread_id: thread.into(),
            revision: rev,
            stale,
            availability: ControlAvailability::Controllable,
            capabilities: CapabilitySet {
                send: true,
                state: true,
                command: true,
                permissions: false,
                blocking_input: true,
                interrupt: false,
            },
            runtime_status: "idle".into(),
            content_state: ContentState::Ready,
            messages: Vec::new(),
            truncated: false,
            history_token: String::new(),
            pending: Vec::new(),
            async_questions: Vec::new(),
            effective_operations:
                crate::services::codex_desktop_ipc::capability::EffectiveOperations::unknown(),
            probe_status: ProbeStatus::idle(),
            queue_paused: false,
            owner_epoch: epoch,
            owner_generation: gen,
            local_seq: 0,
            projection_version: None,
            active_turn_id: None,
        }
    }

    /// 终审 R5：同一 revision 下，启动缓冲里的**旧「启用」策略帧**（序号较高）
    /// 不得覆盖「关闭后重算」的更新首快照——快照以当前 projection_seq 盖章后，
    /// 序号轴上它 ≥ 一切先行帧，合并门必须丢弃旧帧（此后无 Desktop 帧也不收敛
    /// 出旧按钮）。
    #[test]
    fn startup_merge_drops_stale_enabled_buffer_under_fresh_disabled_snapshot() {
        let mut old_enabled = p("t", 1, 1, Some(7), false);
        old_enabled.local_seq = 5;
        let mut fresh_disabled = p("t", 1, 1, Some(7), false);
        // 快照以当前 projection_seq.load() 盖章（≥ 已发布的 5 号帧）
        fresh_disabled.local_seq = 7;
        let merged = merge_startup_buffer(&k("t"), fresh_disabled, vec![old_enabled]);
        assert_eq!(
            merged.len(),
            1,
            "旧启用策略帧必须被丢弃：首快照（更新策略）为准"
        );
        assert_eq!(merged[0].local_seq, 7);
    }

    /// 终审 F5：Desktop 侧标识全同（epoch/generation/revision/stale 一致）时，
    /// **本地序号**决胜——本地策略更新（schema 证据安装/恢复的重发布）不被
    /// 「同 revision」吞掉。
    #[test]
    fn local_seq_breaks_same_revision_ties_for_local_republish() {
        let mut a = p("t", 1, 1, Some(5), false);
        let mut b = p("t", 1, 1, Some(5), false);
        a.local_seq = 3;
        b.local_seq = 7;
        assert!(
            supersedes(&b, &a),
            "同 revision：更高 local_seq 的本地重发布必须胜出"
        );
        assert!(!supersedes(&a, &b), "旧事件的序号更小——不得反超（不回退）");
        // 零序号（手工构造/旧夹具）与有序号：同 revision 下仍按序号竞争
        let mut c = p("t", 1, 1, Some(5), false);
        c.local_seq = 1;
        assert!(!supersedes(&c, &b), "零/低序号不得覆盖高序号");
    }

    /// 终审 F5：Desktop revision 竞争**优先**于本地序号——更高的 revision 仍是
    /// 新世界，本地序号不越权（不伪造 revision 的对称面：真新帧不被旧序号挤掉）。
    #[test]
    fn higher_revision_still_wins_over_local_seq() {
        let mut old_world_high_seq = p("t", 1, 1, Some(5), false);
        old_world_high_seq.local_seq = 99;
        let new_world = p("t", 1, 1, Some(6), false);
        assert!(
            supersedes(&new_world, &old_world_high_seq),
            "更高 revision 是新世界，胜过高本地序号"
        );
    }

    /// 交界处发布最终态：缓冲里更新的 state 必须排在快照之后被补发。
    #[test]
    fn buffered_final_state_survives_after_snapshot() {
        let key = k("t");
        let merged = merge_startup_buffer(
            &key,
            p("t", 1, 1, Some(5), false),
            vec![p("t", 1, 1, Some(6), false)],
        );
        let revs: Vec<_> = merged.iter().map(|x| x.revision).collect();
        assert_eq!(revs, vec![Some(5), Some(6)], "快照在前，新事件补发");
    }

    /// 旧缓冲不得覆盖较新首快照。
    #[test]
    fn older_buffered_does_not_override_snapshot() {
        let key = k("t");
        let merged = merge_startup_buffer(
            &key,
            p("t", 1, 1, Some(9), false),
            vec![p("t", 1, 1, Some(8), false)],
        );
        assert_eq!(merged.len(), 1, "旧事件丢弃");
        assert_eq!(merged[0].revision, Some(9));
    }

    /// owner 换代：generation 更高的低 revision 必须接受；旧 epoch 的高 revision 必须拒。
    #[test]
    fn owner_generation_wins_over_revision() {
        let snap = p("t", 2, 3, Some(50), false);
        assert!(
            !supersedes(&p("t", 1, 9, Some(99), false), &snap),
            "旧 epoch 的高 revision 不是新世界"
        );
        assert!(
            supersedes(&p("t", 2, 4, Some(1), false), &snap),
            "同 epoch 新一代 generation 即使 revision 更低也是合法新状态"
        );
        assert!(
            supersedes(&p("t", 3, 0, None, false), &snap),
            "新 epoch 本身更新"
        );
    }

    /// stale 事件不得覆盖新鲜快照；无 revision 的 stale 也不得排在其后。
    #[test]
    fn stale_buffered_is_delivered_not_dropped() {
        let snap = p("t", 1, 1, Some(5), false);
        // 2026-10-20 复审 P2：stale 是**失效信号**——同/更高 revision 必达；
        // revision=None 的 invalidate 同样必达（无内容，只把下游标成 stale）。
        assert!(supersedes(&p("t", 1, 1, Some(6), true), &snap));
        let merged = merge_startup_buffer(&k("t"), snap, vec![p("t", 1, 1, None, true)]);
        assert_eq!(merged.len(), 2, "不能丢弃断线/invalidate 事件");
        assert!(merged[1].stale);
    }

    /// P1：回执 Turn 关联判定——派发前已存在的旧 Turn 必须拒绝；缺 turnId 保守拒绝。
    #[test]
    fn turn_association_refusal_classifies_stale_and_unconfirmed() {
        let mut existing = std::collections::HashSet::new();
        existing.insert("turn-old".to_string());
        assert_eq!(
            turn_association_refusal(&existing, Some("turn-old")),
            Some("stale_turn_echo"),
            "旧 Turn 回显不得接受"
        );
        assert_eq!(
            turn_association_refusal(&existing, Some("turn-new")),
            None,
            "新 Turn 正常接受"
        );
        assert_eq!(
            turn_association_refusal(&existing, None),
            Some("turn_unconfirmed"),
            "缺 turnId 无法确认关联 → 保守 Unknown"
        );
    }

    /// P2：首快照后唯一断线事件（同代际、同 revision、stale=true）必须送达，
    /// 不得被 `supersedes` 的「stale 不覆盖新鲜」规则丢弃。
    #[test]
    fn stale_disconnect_event_at_same_revision_is_delivered() {
        let key = k("t");
        let merged = merge_startup_buffer(
            &key,
            p("t", 1, 1, Some(7), false),
            vec![p("t", 1, 1, Some(7), true)],
        );
        assert_eq!(merged.len(), 2, "断线无效化事件必须送达");
        assert!(merged[1].stale);
    }

    /// P2：Lagged 恢复期间收到的多帧按统一顺序判据合并——旧 revision 丢弃、
    /// 新 revision 保留，最终状态不回退。
    #[test]
    fn lagged_recovery_frames_do_not_regress_snapshot() {
        let key = k("t");
        let merged = merge_startup_buffer(
            &key,
            p("t", 1, 1, Some(7), false),
            vec![
                p("t", 1, 1, Some(6), false),
                p("t", 1, 1, Some(8), false),
                p("t", 1, 1, Some(7), false),
            ],
        );
        assert_eq!(merged.len(), 2, "只保留真正更新的一帧");
        assert_eq!(merged.last().unwrap().revision, Some(8));
    }

    /// P2：stale 快照只能由更高 revision（或新 owner）清除，同 revision 的
    /// 非 stale 不能空口恢复。
    #[test]
    fn stale_snapshot_cleared_only_by_newer_revision() {
        let stale = p("t", 1, 1, Some(7), true);
        assert!(!supersedes(&p("t", 1, 1, Some(7), false), &stale));
        assert!(supersedes(&p("t", 1, 1, Some(8), false), &stale));
        assert!(
            supersedes(&p("t", 2, 0, Some(1), false), &stale),
            "换 owner 新世代"
        );
    }

    /// 别的 Thread 的缓冲事件一律忽略（单会话订阅不串台）。
    #[test]
    fn other_thread_events_are_ignored() {
        let merged = merge_startup_buffer(
            &k("mine"),
            p("mine", 1, 1, Some(1), false),
            vec![p("other", 2, 2, Some(9), false)],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].thread_id, "mine");
    }
}

// ============================================================
// T07/T08：POST /codex/async-answer（异步正式答题；独立动作类别）
// ============================================================

/// H05：占题守卫——从取得占用到「交给 router 登记/派发」之间的任何早退（零预算、
/// 越队授权写失败、设备撤销等）都在 Drop 时释放占用，避免把题永久锁成
/// `question_occupied`（无动作行可核对）。
struct QuestionClaimGuard {
    store: std::sync::Arc<crate::services::codex_control::ActionStore>,
    action_id: String,
    thread_id: String,
    claim_owner: String,
    question_keys: Vec<String>,
    armed: bool,
}

impl QuestionClaimGuard {
    fn new(
        store: std::sync::Arc<crate::services::codex_control::ActionStore>,
        action_id: &str,
        thread_id: &str,
        claim_owner: &str,
        question_keys: Vec<String>,
    ) -> Self {
        Self {
            store,
            action_id: action_id.to_string(),
            thread_id: thread_id.to_string(),
            claim_owner: claim_owner.to_string(),
            question_keys,
            armed: true,
        }
    }

    /// 交给 router 之后由「落账 + 占用」同一事务接管。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for QuestionClaimGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Err(e) = self.store.settle_group_claim_keys(
                &self.thread_id,
                &self.action_id,
                &self.claim_owner,
                &self.question_keys,
                "not_sent",
            ) {
                log::error!(
                    "codex/async-answer 早退释放占用失败 action={} err={e}",
                    self.action_id
                );
            }
        }
    }
}

/// N02/N04：异步答题的**效果观察拥有者**（从派发起即运行；HTTP 取消/前台
/// Unknown 不影响）。只有同时满足「原动作 Accepted」「原会话题已 Answered」「该
/// 答复所在 Turn 含本动作持久 cmid 的正式 userMessage」才结算一次成功。
fn spawn_async_effect_observer(
    hub: std::sync::Arc<CodexControlHub>,
    key: crate::services::codex_desktop_ipc::types::ThreadKey,
    action_id: String,
    question_key: String,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            let view = hub.router.store().get_for_device_unisolated(&action_id);
            if let Some(view) = view {
                match view.state {
                    crate::services::codex_desktop_ipc::types::ActionState::Rejected
                    | crate::services::codex_desktop_ipc::types::ActionState::Cancelled => return,
                    crate::services::codex_desktop_ipc::types::ActionState::Accepted => {
                        let receipt_turn = view.receipt.as_ref().and_then(|r| r.turn_id.clone());
                        if let (Some(turn), Some((_, cmid))) = (
                            receipt_turn,
                            hub.router.store().action_thread_and_cmid(&action_id),
                        ) {
                            let answered = hub
                                .adapter
                                .projection(&key)
                                .await
                                .async_questions
                                .iter()
                                .any(|question| {
                                    question.question_key == question_key
                                        && question.state
                                            == crate::services::codex_desktop_ipc::async_question::InteractionState::Answered
                                });
                            if answered
                                && hub
                                    .adapter
                                    .turn_has_client_message(&key, &turn, &cmid)
                                    .await
                            {
                                hub.settle_confirmed_effect(&action_id);
                                return;
                            }
                        }
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });
}

/// POST /codex/async-answer 请求体（严格 camelCase）。
///
/// **只接这三个字段 + 暂停越队选择 + 前台预算**：`raw` / `owner` / `method` /
/// `attachments` 等任何多余键 → 400（客户端不能借正式答题权限发任意正文）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AsyncAnswerReq {
    pub session_id: String,
    pub action_id: String,
    pub question_key: String,
    pub context_token: String,
    pub answer: String,
    /// 队列暂停时"仅本条先发"（一次性 grant 绑定本 action；默认 false）。
    #[serde(default)]
    pub bypass_paused_queue: bool,
    /// 前台剩余预算（毫秒；省略 = 30_000，上限夹到 30_000）。
    #[serde(default)]
    pub front_budget_ms: Option<u64>,
}

/// POST /m/api/v1/codex/async-answer：空闲异步题的**正式答题**。
///
/// 与 `pending-response` 同源的鉴权/身份/限流防线；动作类别独立（不进普通消息
/// FIFO），回执直接是 `ActionReceipt`（不返回 Queued）。
pub async fn async_answer(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<AsyncAnswerReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    // H04：前台预算自**入口**起算（会话扫描/投影/占题都消耗同一预算）；客户端传
    // 的是剩余值，仍夹上限。
    let front_budget_ms = req
        .front_budget_ms
        .unwrap_or(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS)
        .min(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS);
    let front = crate::services::codex_desktop_ipc::budget::Deadline::after_ms(front_budget_ms);
    // H01：**入口**捕获目标（后续投影/占题/派发期间即使换目标，也只结算这一个）。
    let effect_target = st.codex_control.capture_observed_target();
    let hub = &st.codex_control;
    if !hub.state_available() {
        return capability_unavailable();
    }
    let sid = req.session_id.trim().to_string();
    let action_id = req.action_id.trim().to_string();
    // J02：本次调用独立的占用拥有令牌（并发同 actionId 不同调用不能接管/清理）。
    let claim_owner = crate::services::codex_desktop_ipc::types::new_uuid();
    let question_key = req.question_key.trim().to_string();
    let context_token = req.context_token.trim().to_string();
    if sid.is_empty() || action_id.is_empty() || question_key.is_empty() || context_token.is_empty()
    {
        return bad_request();
    }
    let Some((device_id, device_name)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/async-answer 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    // R02：同题**持久排他**——在任何派发之前用账本 CAS 裁决（跨 actionId／设备／
    // 重启）。题已不在当前投影 → 旧上下文（零占用写入）；已见正式答复 → 200 明确
    // 零投递；被其他动作占用（在途/未知）→ 409（客户端不得换新 ID 重试）。
    let projection = hub.adapter.projection(&key).await;
    let Some(question) = projection
        .async_questions
        .iter()
        .find(|question| question.question_key == question_key)
    else {
        return error_body(StatusCode::CONFLICT, "stale_context", None);
    };
    let answered = question.state
        == crate::services::codex_desktop_ipc::async_question::InteractionState::Answered;
    // G05/G08：旧单题入口同样先核完整载荷身份——复用 ID 的跨题/跨设备请求不能
    // 先占用再清理；同载荷重放（即使原题已答）只读返回原账本。
    let single_payload = crate::services::codex_control::ActionPayload::Text(req.answer.clone());
    let single_hash = crate::services::codex_control::payload_hash(
        ActionKind::AsyncQuestion,
        &key,
        Some(&question_key),
        Some(&context_token),
        &single_payload,
    );
    if let Some((owner_device, owner_thread, owner_hash, snapshot)) =
        hub.router.store().action_identity(&action_id)
    {
        if owner_device != device_id || owner_thread != key.thread_id || owner_hash != single_hash {
            return error_body(StatusCode::CONFLICT, "action_conflict", None);
        }
        if let Some(receipt) = snapshot.receipt {
            return respond_receipt_response(receipt);
        }
        return json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "actionId": action_id,
                "delivery": "unknown_delivery",
                "route": "desktop_ipc",
                "turnId": null,
                "envelopeId": null,
                "reasonCode": "unknown_delivery",
            }),
        );
    }
    // H01：**不**在这里按“当前目标”记成功——历史已答题不能给当前 build 授权；
    // 成功只由原动作的捕获目标在效果确认后结算（见下方 Accepted 后的观察任务）。
    let owner_token = format!(
        "{}:{}:{}",
        hub.owner_boot(),
        projection.owner_epoch,
        projection.owner_generation
    );
    let claim_outcome = hub.router.store().claim_question(
        &key.thread_id,
        &question_key,
        &action_id,
        &claim_owner,
        &crate::database::dao::codex_interactions::ClaimFacts {
            context_token: Some(&context_token),
            answered_in_projection: answered,
            now_ms: 0,
            epoch: projection.owner_epoch,
            owner_token: &owner_token,
        },
    );
    // H05：只有**本次新取得**的同题占用才允许在本动作结算时收尾；同 ID 重放
    //（AlreadyMine）与既有动作的占用绝不由本调用改写。
    let newly_claimed = matches!(
        claim_outcome,
        Ok(crate::database::dao::codex_interactions::QuestionClaim::Claimed)
    );
    match claim_outcome {
        Ok(crate::database::dao::codex_interactions::QuestionClaim::Claimed)
        | Ok(crate::database::dao::codex_interactions::QuestionClaim::AlreadyMine) => {}
        Ok(crate::database::dao::codex_interactions::QuestionClaim::Answered) => {
            let receipt = ActionReceipt {
                action_id: action_id.clone(),
                delivery: Delivery::NotSent,
                route: crate::services::codex_desktop_ipc::types::ControlRoute::None,
                turn_id: None,
                envelope_id: None,
                reason_code: Some("not_actionable".into()),
            };
            let _ = hub.router.store().settle_group_claim_keys(
                &key.thread_id,
                &action_id,
                &claim_owner,
                std::slice::from_ref(&question_key),
                receipt.delivery.as_str(),
            );
            return respond_receipt_response(receipt);
        }
        Ok(crate::database::dao::codex_interactions::QuestionClaim::Occupied { .. }) => {
            return error_body(StatusCode::CONFLICT, "question_occupied", None);
        }
        Err(e) => {
            log::error!("codex/async-answer 同题占用写入失败（零派发）: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    }
    // N02/N04：效果观察归派发拥有者——从取得占用起运行，HTTP 取消/前台 Unknown
    // 不销毁；只有强证明（Accepted + Answered + 本动作 cmid 的正式消息）才结算。
    spawn_async_effect_observer(
        hub.clone(),
        key.clone(),
        action_id.clone(),
        question_key.clone(),
    );
    // H05：从这里到 router 的早退都必须释放占用（Drop 兜底）。
    let mut claim_guard = QuestionClaimGuard::new(
        hub.router.store().clone(),
        &action_id,
        &key.thread_id,
        &claim_owner,
        vec![question_key.clone()],
    );
    let command = crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand {
        question_key: question_key.clone(),
        context_token,
        answer: req.answer.clone(),
        bypass_paused_queue: req.bypass_paused_queue,
        answers: Vec::new(),
        acknowledged_answered: Vec::new(),
    };
    // "仅本条先发" = 一次性越队授权（绑定本 action；消费即失效）。
    if req.bypass_paused_queue {
        if let Err(e) = hub.router.store().create_bypass_grant(&action_id, &key) {
            log::error!("codex/async-answer 越队授权写入失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    }
    let ctx = ActionContext {
        action_id: action_id.clone(),
        device_id,
        device_name,
        kind: ActionKind::AsyncQuestion,
        key,
    };
    claim_guard.disarm();
    let outcome = hub
        .router
        .answer_async(
            ctx.clone(),
            command,
            front,
            crate::services::codex_control::router::EffectSettlement {
                target: effect_target,
                on_settled: None,
            },
        )
        .await;
    if newly_claimed {
        if let RouterOutcome::Completed(receipt) = &outcome {
            // H05：覆盖 router 预派发早退（未登记/到期）——只收尾本次新占用的题；
            // 真实未知（unknown）保持排他，明确零投递才释放。
            let _ = hub.router.store().settle_group_claim_keys(
                &ctx.key.thread_id,
                &action_id,
                &claim_owner,
                std::slice::from_ref(&question_key),
                receipt.delivery.as_str(),
            );
        }
    }
    match outcome {
        RouterOutcome::Completed(receipt) => {
            let result = audit_result_of_receipt(&receipt);
            codex_audit(
                &st,
                &ctx,
                AUDIT_ANSWER,
                &session.id,
                Some(&question_key),
                receipt.envelope_id.as_deref(),
                &result,
            );
            respond_receipt_response(receipt)
        }
        RouterOutcome::Queued { .. } => {
            // 异步答题**永不进 FIFO**；Queued 只可能是同 action 在途（Reserved）。
            error_body(StatusCode::CONFLICT, "action_conflict", None)
        }
    }
}

/// B05：`POST /codex/async-answer-group` 请求体（严格 camelCase）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsyncAnswerGroupReq {
    pub session_id: String,
    pub action_id: String,
    pub answers: Vec<GroupAnswerEntryReq>,
    /// G08：用户显式复核后可跳过“已被别端正式回答”的题（必须精确列出）。
    #[serde(default)]
    pub acknowledged_answered: Vec<String>,
    #[serde(default)]
    pub front_budget_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupAnswerEntryReq {
    pub question_key: String,
    pub context_token: String,
    pub answer: String,
}

/// B05/B06：同组多题**一次动作、一次正式交付**。
///
/// 与单题入口同源鉴权/身份/预算；差异：
/// * 服务端从当前投影验证**整组同源、成员齐全、无别端已答**，客户端不能传 raw；
/// * 整组占用在一个事务内取得，动作收尾按同一 action_id 释放；
/// * 队列暂停时本入口自动绑定“仅本组先发”的一次性授权（旧消息保持暂停），
///   不需要用户额外点按钮；运行中/原生阻塞/证据不足仍由 router/adapter 拦下。
pub async fn async_answer_group(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<AsyncAnswerGroupReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let front_budget_ms = req
        .front_budget_ms
        .unwrap_or(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS)
        .min(crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS);
    let front = crate::services::codex_desktop_ipc::budget::Deadline::after_ms(front_budget_ms);
    let effect_target = st.codex_control.capture_observed_target();
    let hub = &st.codex_control;
    if !hub.state_available() {
        return capability_unavailable();
    }
    let sid = req.session_id.trim().to_string();
    let action_id = req.action_id.trim().to_string();
    // J02：同一动作的并发请求各有独立占用令牌。
    let claim_owner = crate::services::codex_desktop_ipc::types::new_uuid();
    if sid.is_empty() || action_id.is_empty() || req.answers.is_empty() {
        return bad_request();
    }
    if req.answers.len() > 64 {
        return bad_request();
    }
    let mut seen = std::collections::BTreeSet::new();
    for entry in &req.answers {
        if entry.question_key.trim().is_empty()
            || entry.context_token.trim().is_empty()
            || entry.answer.chars().count()
                > crate::services::codex_desktop_ipc::async_question::MAX_ANSWER_CHARS
            || !seen.insert(entry.question_key.trim().to_string())
        {
            return bad_request();
        }
    }
    let Some((device_id, device_name)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/async-answer-group 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    // G05/G08：鉴权/身份后先用**完整规范载荷**识别既有动作（不依赖当前投影的
    // answered 状态）——同设备同载荷重放只读返回账本，复用 ID 冲突直接拒绝。
    let mut ordered_req: Vec<GroupAnswerEntryReq> = req.answers.clone();
    ordered_req.sort_by_key(|entry| {
        crate::services::codex_desktop_ipc::async_question::parse_question_key(&entry.question_key)
            .map(|(_, index)| index)
            .unwrap_or(0)
    });
    let first_key = ordered_req[0].question_key.trim().to_string();
    let first_token = ordered_req[0].context_token.trim().to_string();
    let entries_all: Vec<_> = ordered_req
        .iter()
        .map(
            |entry| crate::services::codex_desktop_ipc::async_question::AsyncAnswerEntry {
                question_key: entry.question_key.trim().to_string(),
                context_token: entry.context_token.trim().to_string(),
                answer: entry.answer.clone(),
            },
        )
        .collect();
    let payload_material = {
        let probe = crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand {
            question_key: first_key.clone(),
            context_token: first_token.clone(),
            answer: ordered_req[0].answer.clone(),
            bypass_paused_queue: false,
            answers: entries_all,
            acknowledged_answered: Vec::new(),
        };
        probe.payload_material()
    };
    let payload = crate::services::codex_control::ActionPayload::Text(payload_material);
    let hash = crate::services::codex_control::payload_hash(
        ActionKind::AsyncQuestion,
        &key,
        Some(&first_key),
        Some(&first_token),
        &payload,
    );
    if let Some((owner_device, owner_thread, owner_hash, snapshot)) =
        hub.router.store().action_identity(&action_id)
    {
        if owner_device != device_id || owner_thread != key.thread_id || owner_hash != hash {
            return error_body(StatusCode::CONFLICT, "action_conflict", None);
        }
        if let Some(receipt) = snapshot.receipt {
            return respond_receipt_response(receipt);
        }
        return json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "actionId": action_id,
                "delivery": "unknown_delivery",
                "route": "desktop_ipc",
                "turnId": null,
                "envelopeId": null,
                "reasonCode": "unknown_delivery",
            }),
        );
    }
    let projection = hub.adapter.projection(&key).await;
    // 组答题的实际答案必须逐题对得上**当前投影**；任何一题缺失/换 token 都按
    // 过期上下文拒绝，绝不把旧答案套到新题。
    let mut resolved = Vec::with_capacity(req.answers.len());
    for entry in &req.answers {
        let qkey = entry.question_key.trim();
        let Some(question) = projection
            .async_questions
            .iter()
            .find(|question| question.question_key == qkey)
        else {
            return error_body(StatusCode::CONFLICT, "stale_context", None);
        };
        if question.context_token != entry.context_token.trim() {
            return error_body(StatusCode::CONFLICT, "stale_context", None);
        }
        resolved.push((question.clone(), entry.answer.clone()));
    }
    resolved.sort_by_key(|(question, _)| question.question_index);
    let source = {
        let first = &resolved[0].0;
        (first.source_turn_id.clone(), first.source_item_id.clone())
    };
    let mut group_all: Vec<_> = projection
        .async_questions
        .iter()
        .filter(|question| {
            question.source_turn_id == source.0 && question.source_item_id == source.1
        })
        .cloned()
        .collect();
    group_all.sort_by_key(|question| question.question_index);
    // G06：必须与服务端目标组**精确成员集合**相等，不能只比题数。
    let group_keys: std::collections::BTreeSet<String> = group_all
        .iter()
        .map(|question| question.question_key.clone())
        .collect();
    let submitted_keys: std::collections::BTreeSet<String> = resolved
        .iter()
        .map(|(question, _)| question.question_key.clone())
        .collect();
    let answered_keys: Vec<String> = group_all
        .iter()
        .filter(|question| {
            question.state
                == crate::services::codex_desktop_ipc::async_question::InteractionState::Answered
        })
        .map(|question| question.question_key.clone())
        .collect();
    // G08：别端先答部分题时，只有用户显式复核（acknowledgedAnswered 精确列出
    // 已确认题）才允许提交剩余未答部分；首次默认整组拒绝。
    if !answered_keys.is_empty() {
        let ack: std::collections::BTreeSet<String> = req
            .acknowledged_answered
            .iter()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .collect();
        let answered_set: std::collections::BTreeSet<String> =
            answered_keys.iter().cloned().collect();
        if ack != answered_set {
            return json_no_store(
                StatusCode::CONFLICT,
                serde_json::json!({
                    "error": "question_already_answered",
                    "questionKeys": answered_keys,
                }),
            );
        }
        let live_keys: std::collections::BTreeSet<String> =
            group_keys.difference(&answered_set).cloned().collect();
        if live_keys != submitted_keys {
            return json_no_store(
                StatusCode::CONFLICT,
                serde_json::json!({
                    "error": "answers_mismatch",
                    "questionKeys": live_keys,
                }),
            );
        }
    }
    // 无已答题时，提交集合必须与目标组**精确一致**（G06）；有已答题时上面已
    // 按“整组 − 已确认已答”校验。
    if answered_keys.is_empty() && group_keys != submitted_keys {
        let missing: Vec<String> = group_keys.difference(&submitted_keys).cloned().collect();
        let extra: Vec<String> = submitted_keys.difference(&group_keys).cloned().collect();
        return json_no_store(
            StatusCode::CONFLICT,
            serde_json::json!({
                "error": "answers_mismatch",
                "questionKeys": missing,
                "unexpectedQuestionKeys": extra,
            }),
        );
    }
    if group_all.iter().any(|question| {
        matches!(
            question.state,
            crate::services::codex_desktop_ipc::async_question::InteractionState::Ended
                | crate::services::codex_desktop_ipc::async_question::InteractionState::Cancelled
                | crate::services::codex_desktop_ipc::async_question::InteractionState::Rejected
        )
    }) {
        return error_body(StatusCode::CONFLICT, "not_actionable", None);
    }
    let queue_paused = hub
        .router
        .store()
        .queue_pause(&key)
        .ok()
        .flatten()
        .is_some();
    let paused_only = queue_paused
        && resolved.iter().all(|(question, _)| {
            question.state
                == crate::services::codex_desktop_ipc::async_question::InteractionState::TemporarilyBlocked
                && question.disabled_reason_code.as_deref() == Some("operation_queue_paused")
        });
    if resolved
        .iter()
        .any(|(question, _)| !question.actionable && !paused_only)
    {
        let reason = resolved
            .iter()
            .find(|(question, _)| !question.actionable && !paused_only)
            .and_then(|(question, _)| question.disabled_reason_code.as_deref())
            .unwrap_or("not_actionable");
        return error_body(StatusCode::CONFLICT, reason, None);
    }
    // 整组一次占用（事务）；任何一题已被别端占用/解答 → 整组拒绝，零帧。
    let owner_token = format!(
        "{}:{}:{}",
        hub.owner_boot(),
        projection.owner_epoch,
        projection.owner_generation
    );
    let claim_entries: Vec<_> = resolved
        .iter()
        .map(
            |(question, _)| crate::database::dao::codex_interactions::GroupClaimEntry {
                question_key: &question.question_key,
                context_token: Some(question.context_token.as_str()),
                answered_in_projection: false,
            },
        )
        .collect();
    let claim_outcome = hub.router.store().claim_question_group(
        &key.thread_id,
        &action_id,
        &claim_owner,
        &claim_entries,
        &crate::database::dao::codex_interactions::ClaimFacts {
            context_token: None,
            answered_in_projection: false,
            now_ms: 0,
            epoch: projection.owner_epoch,
            owner_token: &owner_token,
        },
    );
    let mut newly_claimed_keys: Vec<String> = Vec::new();
    let newly_claimed = match claim_outcome {
        Ok(crate::database::dao::codex_interactions::GroupClaimOutcome::Claimed { newly }) => {
            newly_claimed_keys = newly;
            true
        }
        Ok(crate::database::dao::codex_interactions::GroupClaimOutcome::AlreadyMine) => false,
        Ok(crate::database::dao::codex_interactions::GroupClaimOutcome::Answered(keys)) => {
            return json_no_store(
                StatusCode::CONFLICT,
                serde_json::json!({ "error": "question_already_answered", "questionKeys": keys }),
            );
        }
        Ok(crate::database::dao::codex_interactions::GroupClaimOutcome::Occupied {
            question_key,
            ..
        }) => {
            return json_no_store(
                StatusCode::CONFLICT,
                serde_json::json!({ "error": "question_occupied", "questionKeys": [question_key] }),
            );
        }
        Err(e) => {
            log::error!("codex/async-answer-group 整组占用写入失败（零派发）: {e}");

            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    spawn_async_effect_observer(
        hub.clone(),
        key.clone(),
        action_id.clone(),
        first_key.clone(),
    );
    let mut claim_guard = QuestionClaimGuard::new(
        hub.router.store().clone(),
        &action_id,
        &key.thread_id,
        &claim_owner,
        newly_claimed_keys.clone(),
    );
    // B06：暂停时自动绑定“仅本组先发”的一次性授权；旧消息仍暂停。
    if paused_only {
        if let Err(e) = hub.router.store().create_bypass_grant(&action_id, &key) {
            log::error!("codex/async-answer-group 越队授权写入失败: {e}");

            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    }
    let answers: Vec<_> = resolved
        .iter()
        .map(|(question, answer)| {
            crate::services::codex_desktop_ipc::async_question::AsyncAnswerEntry {
                question_key: question.question_key.clone(),
                context_token: question.context_token.clone(),
                answer: answer.clone(),
            }
        })
        .collect();
    let command = crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand {
        question_key: first_key.clone(),
        context_token: first_token,
        answer: resolved[0].1.clone(),
        bypass_paused_queue: paused_only,
        answers,
        acknowledged_answered: req
            .acknowledged_answered
            .iter()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .collect(),
    };
    let ctx = ActionContext {
        action_id: action_id.clone(),
        device_id,
        device_name,
        kind: ActionKind::AsyncQuestion,
        key,
    };
    claim_guard.disarm();
    let outcome = hub
        .router
        .answer_async(
            ctx.clone(),
            command,
            front,
            crate::services::codex_control::router::EffectSettlement {
                target: effect_target,
                on_settled: None,
            },
        )
        .await;
    if newly_claimed {
        if let RouterOutcome::Completed(receipt) = &outcome {
            // G05/J02：只收尾**本次新增且仍属本调用令牌**的占用键。
            let _ = hub.router.store().settle_group_claim_keys(
                &ctx.key.thread_id,
                &action_id,
                &claim_owner,
                &newly_claimed_keys,
                receipt.delivery.as_str(),
            );
        }
    }
    match outcome {
        RouterOutcome::Completed(receipt) => {
            let result = audit_result_of_receipt(&receipt);
            codex_audit(
                &st,
                &ctx,
                AUDIT_ANSWER,
                &session.id,
                Some(&first_key),
                receipt.envelope_id.as_deref(),
                &result,
            );
            respond_receipt_response(receipt)
        }
        RouterOutcome::Queued { .. } => error_body(StatusCode::CONFLICT, "action_conflict", None),
    }
}

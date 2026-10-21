//! 只读探测执行器（#172 P0–P3）：证明「读链可用」，失败时如实分级。
//!
//! 设计要点（设计稿 §4.4 / §4.5 / §10）：
//! * **复用 adapter 的单连接**（P0 由调用方 `ensure_connection` 负责），本模块只在
//!   已建立的连接上跑 P1–P3，绝不另开连接、不碰 mirror、不增加 `FollowGate` 计数；
//! * **预算**：P0/P1/P2/P3 = 1/2/3/3s（总 9s），本模块的硬上限 15s（留 6s 余量给
//!   冷启动/调度抖动）；
//! * **失败分级**：`Failed` = 明确协议/连接形态不可用（不自动重试）；`Inconclusive`
//!   = 没有证据（无 owner / 静默 / 解析不出 / 超时）——按退避重试，**绝不判死**；
//! * **被动广播不算证据**：只认「我方声明 following 之后」到达的、`conversationId`
//!   匹配的 snapshot；更早的帧只说明链路活着；
//! * **探完立即 unfollow**：临时引用，不进 `owners`/`mirrors`（不干扰生产跟随状态）。
//!
//! 三条纪律（施工单 §3）：
//! 1. 本模块**不发任何写帧**（没有 start-turn / 没有审批 / 没有问题回答）；
//! 2. `client-discovery-request` 由 connection 层统一回 `canHandle:false`，探测不掺和；
//! 3. P3 判据只看「结构可解析」——空线程也通过（容器在即证明字段形态没漂）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::database::dao::codex_ipc_probe::ProbeState;
use crate::services::codex_desktop_ipc::connection::{
    ConnectionEvent, ConnectionHandle, RpcRequest,
};
use crate::services::codex_desktop_ipc::types::{Delivery, ThreadKey};

/// P0（connect + initialize）预算。P0 由调用方（adapter）执行，本模块只引用该值。
pub const P0_BUDGET: Duration = Duration::from_secs(1);
/// P1（owner discovery）预算（实测 11–13ms；无 owner ~2s 回 no-client-found）。
pub const P1_BUDGET: Duration = Duration::from_secs(2);
/// P2（following 声明 → snapshot）预算（实测 15–21ms）。
pub const P2_BUDGET: Duration = Duration::from_secs(3);
/// P3（snapshot 解析）预算——本模块里 P3 是纯解析，预算用于「等最后一个 snapshot」。
pub const P3_BUDGET: Duration = Duration::from_secs(3);
/// 单次探测**硬上限**（总预算 9s + 6s 余量；超时记 `inconclusive`，不判死）。
pub const TOTAL_BUDGET: Duration = Duration::from_secs(15);
/// discovery 请求自带超时（镜像 Desktop 自己的 `sendRequest` envelope）。
const DISCOVERY_TIMEOUT_MS: u64 = 2_000;

/// 探测级。`as_str()` 进账本 `probe_stage` 列与日志（`P0`..`P3`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStage {
    P0,
    P1,
    P2,
    P3,
}

impl ProbeStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
            Self::P3 => "P3",
        }
    }
}

/// 探测结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// 读链通过（P3 判据满足）。
    Probed {
        stage: ProbeStage,
        revision: Option<u64>,
        duration_ms: u64,
    },
    /// 明确失败（协议/连接形态不可用）——不自动重试。
    Failed {
        stage: ProbeStage,
        error_code: String,
        duration_ms: u64,
    },
    /// 没有证据（无 owner / 静默 / 解析不出 / 超时）——按退避重试。
    Inconclusive {
        stage: ProbeStage,
        error_code: String,
        duration_ms: u64,
    },
}

impl ProbeOutcome {
    pub fn stage(&self) -> ProbeStage {
        match self {
            Self::Probed { stage, .. }
            | Self::Failed { stage, .. }
            | Self::Inconclusive { stage, .. } => *stage,
        }
    }

    pub fn error_code(&self) -> Option<&str> {
        match self {
            Self::Probed { .. } => None,
            Self::Failed { error_code, .. } | Self::Inconclusive { error_code, .. } => {
                Some(error_code.as_str())
            }
        }
    }

    pub fn duration_ms(&self) -> u64 {
        match self {
            Self::Probed { duration_ms, .. }
            | Self::Failed { duration_ms, .. }
            | Self::Inconclusive { duration_ms, .. } => *duration_ms,
        }
    }

    /// 落账本用的状态（`probed` / `failed` / `inconclusive`）。
    pub fn as_state(&self) -> ProbeState {
        match self {
            Self::Probed { .. } => ProbeState::Probed,
            Self::Failed { .. } => ProbeState::Failed,
            Self::Inconclusive { .. } => ProbeState::Inconclusive,
        }
    }
}

// ============================================================
// 单飞 + 硬上限
// ============================================================

/// 同一 `probe_key` 的探测单飞管理器：leader 真跑，follower await 同一结果。
///
/// 为什么要单飞：多个入口（详情页 / SSE / GET）可能同时触发探测；各跑 15s 既慢又会
/// 重复写账本。单飞后同一 key 同刻只有一条探测在飞。
#[derive(Default)]
pub struct ProbeRunner {
    flights: Mutex<HashMap<String, Arc<Flight>>>,
}

#[derive(Default)]
struct Flight {
    done: tokio::sync::Notify,
    result: Mutex<Option<ProbeOutcome>>,
}

impl Flight {
    fn publish(&self, outcome: ProbeOutcome) {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        self.done.notify_waiters();
    }

    async fn wait(&self) -> ProbeOutcome {
        loop {
            // 先建 notified future 再读结果：避免「读完后、await 前」的丢唤醒
            let notified = self.done.notified();
            if let Some(outcome) = self
                .result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return outcome;
            }
            notified.await;
        }
    }
}

impl ProbeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 单飞执行：`probe` 只在 leader 侧被轮询；follower 直接拿到 leader 的结论。
    /// 整个探测受 [`TOTAL_BUDGET`] 硬上限保护（超时 → `inconclusive: probe_timeout`）。
    pub async fn run<F>(&self, probe_key: &str, probe: F) -> ProbeOutcome
    where
        F: std::future::Future<Output = ProbeOutcome>,
    {
        let (flight, leader) = {
            let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
            match flights.get(probe_key) {
                Some(existing) => (existing.clone(), false),
                None => {
                    let created = Arc::new(Flight::default());
                    flights.insert(probe_key.to_string(), created.clone());
                    (created, true)
                }
            }
        };
        if !leader {
            return flight.wait().await;
        }
        let outcome = match tokio::time::timeout(TOTAL_BUDGET, probe).await {
            Ok(outcome) => outcome,
            Err(_) => ProbeOutcome::Inconclusive {
                stage: ProbeStage::P3,
                error_code: "probe_timeout".into(),
                duration_ms: TOTAL_BUDGET.as_millis() as u64,
            },
        };
        flight.publish(outcome.clone());
        self.flights
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(probe_key);
        outcome
    }

    /// 在飞探测数（测试/排障用）。
    pub fn inflight(&self) -> usize {
        self.flights.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 该 build 是否有探测**在飞**（#172/T9a：探测只读事实的「探测中」信号）。
    ///
    /// 单飞键形如 `{probe_key}|{thread_id}`（同 build 的不同线程各自一条），故按
    /// 前缀匹配。只读查询：**不新增并发结构**（复用既有 `flights` 表），也不改
    /// 单飞语义。
    pub fn is_flight_active(&self, probe_key: &str) -> bool {
        let prefix = format!("{probe_key}|");
        self.flights
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|k| k == probe_key || k.starts_with(&prefix))
    }
}

// ============================================================
// P1–P3：在已建立的连接上探测读链
// ============================================================

/// 探测预算（生产用默认值：P1/P2/P3 = 2/3/3s；测试可收紧到毫秒级，免得真等 6s）。
#[derive(Debug, Clone, Copy)]
pub struct ProbeBudget {
    pub p1: Duration,
    pub p2: Duration,
    pub p3: Duration,
}

impl Default for ProbeBudget {
    fn default() -> Self {
        Self {
            p1: P1_BUDGET,
            p2: P2_BUDGET,
            p3: P3_BUDGET,
        }
    }
}

/// P1–P3 读链探测（复用调用方已建立的连接；P0 = `ensure_connection` 不在本函数内）。
///
/// 通过判据（设计 §4.5，Windows 4894 实测形态）：
/// 1. P1：`thread-owner-discovery` 返回 `handledByClientId`（带 `sourceClientId` +
///    `timeoutMs`，镜像 Desktop 的 sendRequest envelope）；
/// 2. P2：声明 `following:true` 后收到本线程的 `thread-stream-state-changed`；
/// 3. P3：`params.change` 是 snapshot 且 `revision` 数值、`conversationState` 含
///    `turns` / `requests` / `turnHistory` 三个容器（空线程也算通过）。
///
/// 无论成败都会尽力发 `following:false`（临时引用，探完即放）。
pub async fn probe_read(conn: &ConnectionHandle, key: &ThreadKey) -> ProbeOutcome {
    probe_read_with_budget(conn, key, ProbeBudget::default()).await
}

/// P0 收尾：等握手给出**我方 clientId**（`result.clientId`，不是顶层；每次握手都换新）。
/// 这是 P1 envelope 里 `sourceClientId` 的来源，也是「连接真的建起来了」的判据——
/// 不能拿「收到任意帧」当 P0 通过。
pub async fn wait_initialized(conn: &ConnectionHandle, budget: Duration) -> Option<String> {
    let deadline = Instant::now() + budget;
    let mut rx = conn.subscribe();
    loop {
        if let Some(id) = conn.initialized_client() {
            return Some(id);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left.min(Duration::from_millis(50)), rx.recv()).await {
            Ok(Ok(ConnectionEvent::Initialized { client_id, .. })) => return Some(client_id),
            Ok(Ok(ConnectionEvent::Disconnected { .. })) | Ok(Err(_)) => return None,
            Ok(Ok(_)) => continue,
            Err(_) => continue, // 本轮没等到：下一轮先看状态口再等
        }
    }
}

/// 同 [`probe_read`]，预算可注入（测试用；生产不传）。
pub async fn probe_read_with_budget(
    conn: &ConnectionHandle,
    key: &ThreadKey,
    budget: ProbeBudget,
) -> ProbeOutcome {
    let started = Instant::now();
    let elapsed = || started.elapsed().as_millis() as u64;

    // ---- P0 收尾：等握手（clientId 是 P1 envelope 的 sourceClientId 来源）----
    // 握手没回包 = 「不可达」= **没有证据**（不是明确失败：Desktop 可能只是慢/冷启动）。
    if wait_initialized(conn, P0_BUDGET).await.is_none() {
        return ProbeOutcome::Inconclusive {
            stage: ProbeStage::P0,
            error_code: "initialize_timeout".into(),
            duration_ms: elapsed(),
        };
    }

    // ---- P1：owner discovery（`sourceClientId`/`timeoutMs` 走**信封**，见 RpcRequest）----
    match timed(conn_discovery(conn, key), budget.p1).await {
        Ok(Ok(_owner)) => {}
        Ok(Err(stage_err)) => return stage_err.into_outcome(ProbeStage::P1, elapsed()),
        Err(()) => {
            return ProbeOutcome::Inconclusive {
                stage: ProbeStage::P1,
                error_code: "no_client_response".into(),
                duration_ms: elapsed(),
            }
        }
    }

    // ---- P2：临时 follow → 等 snapshot ----
    // 顺序固定为 **subscribe → drain_stale → declare_following → wait_snapshot**：
    // 先把「声明之前」的被动广播丢掉，再把订阅窗口交给「声明之后」的帧——声明后立刻
    // 到达的 snapshot 必须在窗口里（旧顺序 declare→drain 会把它当旧帧丢掉）。
    let mut rx = conn.subscribe();
    drain_stale(&mut rx);
    if let Err(stage_err) = declare_following(conn, key).await {
        best_effort_unfollow(conn, key).await;
        return stage_err.into_outcome(ProbeStage::P2, elapsed());
    }
    let snapshot = match wait_snapshot(conn, &mut rx, key, budget.p2).await {
        Ok(frame) => frame,
        Err(SnapshotMiss::Drift) => {
            // 静默：先补一条 status-requested（Desktop 自己的信封里也这么发），
            // 再用剩余预算等一次；仍静默 → 协议漂移疑似（不误判可用）
            let _ = request_status(conn, key).await;
            match wait_snapshot(conn, &mut rx, key, budget.p3).await {
                Ok(frame) => frame,
                Err(SnapshotMiss::Drift) => {
                    best_effort_unfollow(conn, key).await;
                    return ProbeOutcome::Inconclusive {
                        stage: ProbeStage::P2,
                        error_code: "protocol_drift_suspected".into(),
                        duration_ms: elapsed(),
                    };
                }
                Err(SnapshotMiss::Disconnected(code)) => {
                    best_effort_unfollow(conn, key).await;
                    return ProbeOutcome::Inconclusive {
                        stage: ProbeStage::P2,
                        error_code: code,
                        duration_ms: elapsed(),
                    };
                }
            }
        }
        Err(SnapshotMiss::Disconnected(code)) => {
            best_effort_unfollow(conn, key).await;
            // 断链 = 链路中断（不是协议形态被证伪）→ 按「没有证据」记，可重试
            return ProbeOutcome::Inconclusive {
                stage: ProbeStage::P2,
                error_code: code,
                duration_ms: elapsed(),
            };
        }
    };
    best_effort_unfollow(conn, key).await;

    // ---- P3：结构判据（空线程也通过；解析不出 = inconclusive，不伪造）----
    match snapshot_revision(&snapshot) {
        Some(revision) => ProbeOutcome::Probed {
            stage: ProbeStage::P3,
            revision: Some(revision),
            duration_ms: elapsed(),
        },
        None => ProbeOutcome::Inconclusive {
            stage: ProbeStage::P3,
            error_code: "snapshot_unreadable".into(),
            duration_ms: elapsed(),
        },
    }
}

/// 阶段内错误（用于把「明确失败」与「没有证据」分开）。
struct StageError {
    code: String,
    inconclusive: bool,
}

impl StageError {
    fn into_outcome(self, stage: ProbeStage, duration_ms: u64) -> ProbeOutcome {
        if self.inconclusive {
            ProbeOutcome::Inconclusive {
                stage,
                error_code: self.code,
                duration_ms,
            }
        } else {
            ProbeOutcome::Failed {
                stage,
                error_code: self.code,
                duration_ms,
            }
        }
    }
}

/// 有界执行：`None` = 本阶段预算内没有结果。
async fn timed<F, T>(fut: F, budget: Duration) -> Result<T, ()>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(budget, fut).await.map_err(|_| ())
}

/// P1：`thread-owner-discovery`。`sourceClientId` / `timeoutMs` 走**信封**
/// （与 Desktop 自己的 `sendRequest` 同形；见 [`RpcRequest`] 字段注释），不进 `params`。
///
/// 失败分级：
/// * `no-client-found`（含 router 级无 `method` 响应）→ **不确定**，不判死（版本不匹配
///   与「无 owner」同形）；
/// * 其它明确拒绝（协议错误）→ **失败**；
/// * `NotSent` / `UnknownDelivery`（没送达 / 结果未知）→ **不确定**（链路问题，可重试）。
async fn conn_discovery(conn: &ConnectionHandle, key: &ThreadKey) -> Result<String, StageError> {
    let version = conn
        .version_of("thread-owner-discovery")
        .map_err(|e| StageError {
            code: short_code(&e),
            inconclusive: false, // 方法不在方法表 = 明确协议错（UnsupportedMethod）
        })?;
    let source_client_id = conn.initialized_client().unwrap_or_default();
    let outcome = conn
        .request(RpcRequest {
            method: "thread-owner-discovery".into(),
            params: serde_json::json!({
                "hostId": key.host_id,
                "conversationId": key.thread_id,
            }),
            version: Some(version),
            target: None,
            mutating: false,
            validity: None,
            source_client_id: Some(source_client_id),
            timeout_ms: Some(DISCOVERY_TIMEOUT_MS),
        })
        .await;
    match outcome.delivery {
        Delivery::Accepted => outcome.handled_by.ok_or_else(|| StageError {
            code: "owner_identity_missing".into(),
            inconclusive: true,
        }),
        Delivery::Rejected => {
            let text = outcome
                .result
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();
            Err(StageError {
                // 「无 owner」与「版本不匹配」同形（no-client-found）：只记不确定
                code: if text.contains("no-client-found") {
                    "no-client-found".into()
                } else {
                    "owner_discovery_rejected".into()
                },
                inconclusive: text.contains("no-client-found") || text.is_empty(),
            })
        }
        // 没送达 = 链路问题（不可达/断链）：**没有证据**，不是协议被证伪
        Delivery::NotSent => Err(StageError {
            code: "disconnected".into(),
            inconclusive: true,
        }),
        Delivery::UnknownDelivery => Err(StageError {
            code: "delivery_unknown".into(),
            inconclusive: true,
        }),
    }
}

/// 声明 following（broadcast v1）。方法版本查表失败 = 明确协议错（Failed）；
/// 写失败（断链/预算） = 没有证据（Inconclusive）。
async fn declare_following(conn: &ConnectionHandle, key: &ThreadKey) -> Result<(), StageError> {
    let version = conn
        .version_of("thread-stream-following-changed")
        .map_err(|e| StageError {
            code: short_code(&e),
            inconclusive: false,
        })?;
    conn.broadcast(
        "thread-stream-following-changed",
        version,
        serde_json::json!({
            "conversationId": key.thread_id,
            "hostId": key.host_id,
            "following": true,
        }),
    )
    .await
    .map_err(|e| StageError {
        code: short_code(&e),
        inconclusive: true,
    })
}

/// 立即撤销临时 follow（尽力而为：失败不影响探测结论，也不留本地状态）。
async fn best_effort_unfollow(conn: &ConnectionHandle, key: &ThreadKey) {
    if let Ok(version) = conn.version_of("thread-stream-following-changed") {
        let _ = conn
            .broadcast(
                "thread-stream-following-changed",
                version,
                serde_json::json!({
                    "conversationId": key.thread_id,
                    "hostId": key.host_id,
                    "following": false,
                }),
            )
            .await;
    }
}

/// 补发 `thread-stream-following-status-requested`（v1）：Desktop 自己的信封里也这么
/// 请求首帧；静默时用它区分「慢」与「漂移」。
async fn request_status(conn: &ConnectionHandle, key: &ThreadKey) -> Result<(), String> {
    let version = conn
        .version_of("thread-stream-following-status-requested")
        .map_err(|e| short_code(&e))?;
    conn.broadcast(
        "thread-stream-following-status-requested",
        version,
        serde_json::json!({
            "conversationId": key.thread_id,
            "hostId": key.host_id,
        }),
    )
    .await
    .map_err(|e| short_code(&e))
}

enum SnapshotMiss {
    /// 预算内没等到（静默）——按漂移疑似处理。
    Drift,
    /// 事件通道断了（连接结束）。
    Disconnected(String),
}

/// 等本线程的 `thread-stream-state-changed`（只认 `conversationId` 匹配的帧）。
async fn wait_snapshot(
    conn: &ConnectionHandle,
    rx: &mut tokio::sync::broadcast::Receiver<ConnectionEvent>,
    key: &ThreadKey,
    budget: Duration,
) -> Result<Value, SnapshotMiss> {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(SnapshotMiss::Drift);
        }
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(ConnectionEvent::Broadcast { frame, .. })) => {
                if is_thread_state_changed(&frame, &key.thread_id) {
                    return Ok(frame);
                }
            }
            Ok(Ok(ConnectionEvent::Disconnected { .. })) => {
                return Err(SnapshotMiss::Disconnected("disconnected".into()))
            }
            Ok(Ok(_)) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => return Err(SnapshotMiss::Disconnected("event_channel_closed".into())),
            Err(_) => {
                // 连接关闭口也可查（事件可能早于订阅发生）
                if conn.is_closed() {
                    return Err(SnapshotMiss::Disconnected("disconnected".into()));
                }
                return Err(SnapshotMiss::Drift);
            }
        }
    }
}

/// 丢掉订阅缓冲里的旧帧（声明 following 之前到达的广播**不算我方证据**）。
fn drain_stale(rx: &mut tokio::sync::broadcast::Receiver<ConnectionEvent>) {
    loop {
        match rx.try_recv() {
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
        }
    }
}

/// 是否本线程的状态帧。
fn is_thread_state_changed(frame: &Value, thread_id: &str) -> bool {
    frame.get("method").and_then(|m| m.as_str()) == Some("thread-stream-state-changed")
        && frame
            .get("params")
            .and_then(|p| p.get("conversationId"))
            .and_then(|c| c.as_str())
            == Some(thread_id)
}

/// P3 判据（纯函数）：`change.type == "snapshot"` + `revision` 数值 +
/// `conversationState` 含 `turns`(array) / `requests`(array) / `turnHistory`(object)。
/// 通过 → `Some(revision)`；不通过 → `None`（**不伪造空会话**）。
///
/// 空线程也算通过：三个容器在即证明结构可解析，不要求有内容（`entitiesByKey=0` 正常）。
pub fn snapshot_revision(frame: &Value) -> Option<u64> {
    let change = frame.get("params")?.get("change")?;
    if change.get("type").and_then(|t| t.as_str()) != Some("snapshot") {
        return None;
    }
    let revision = change.get("revision")?.as_u64()?;
    let state = change.get("conversationState")?;
    let state = state.as_object()?;
    let turns_ok = state.get("turns").is_some_and(|v| v.is_array());
    let requests_ok = state.get("requests").is_some_and(|v| v.is_array());
    let history_ok = state.get("turnHistory").is_some_and(|v| v.is_object());
    (turns_ok && requests_ok && history_ok).then_some(revision)
}

/// 错误 → 机器码（复用既有协议错误码词表，不另造词）。
fn short_code(err: &crate::services::codex_desktop_ipc::types::ProtocolError) -> String {
    err.code().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_frame(thread_id: &str, revision: u64) -> Value {
        serde_json::json!({
            "type": "broadcast",
            "method": "thread-stream-state-changed",
            "params": {
                "conversationId": thread_id,
                "change": {
                    "type": "snapshot",
                    "revision": revision,
                    "conversationState": {
                        "id": thread_id,
                        "turns": [],
                        "requests": [],
                        "turnHistory": { "kind": "canonical", "history": {} }
                    }
                }
            }
        })
    }

    /// P3 判据：Windows 4894 实测形态通过（空线程也算通过）。
    #[test]
    fn snapshot_criteria_accepts_observed_shape() {
        assert_eq!(
            snapshot_revision(&snapshot_frame("t1", 5)),
            Some(5),
            "空线程只要三个容器在就通过"
        );
    }

    /// 字段缺失 / 改名 / 类型漂移 → None（降级但不伪造）。
    #[test]
    fn snapshot_criteria_rejects_drifted_shapes() {
        let mut cases = Vec::new();
        // type 不是 snapshot
        let mut v = snapshot_frame("t1", 1);
        v["params"]["change"]["type"] = serde_json::json!("patches");
        cases.push(v);
        // revision 不是数值
        let mut v = snapshot_frame("t1", 1);
        v["params"]["change"]["revision"] = serde_json::json!("5");
        cases.push(v);
        // turns 缺失
        let mut v = snapshot_frame("t1", 1);
        v["params"]["change"]["conversationState"]
            .as_object_mut()
            .unwrap()
            .remove("turns");
        cases.push(v);
        // turnHistory 改名
        let mut v = snapshot_frame("t1", 1);
        let cs = v["params"]["change"]["conversationState"]
            .as_object_mut()
            .unwrap();
        cs.insert("turn_history".into(), serde_json::json!({}));
        cs.remove("turnHistory");
        cases.push(v);
        // requests 类型漂移
        let mut v = snapshot_frame("t1", 1);
        v["params"]["change"]["conversationState"]["requests"] = serde_json::json!({});
        cases.push(v);
        // 完全不像的状态帧
        cases.push(serde_json::json!({ "method": "thread-stream-state-changed" }));
        for (idx, frame) in cases.iter().enumerate() {
            assert_eq!(snapshot_revision(frame), None, "case {idx} 必须拒绝");
        }
    }

    /// 只认本线程的状态帧（别的线程的 snapshot 不能当证据）。
    #[test]
    fn only_matching_thread_frames_are_candidates() {
        let frame = snapshot_frame("t1", 1);
        assert!(is_thread_state_changed(&frame, "t1"));
        assert!(!is_thread_state_changed(&frame, "t2"));
        let mut other = frame.clone();
        other["method"] = serde_json::json!("thread-stream-following-changed");
        assert!(!is_thread_state_changed(&other, "t1"), "别的 method 不算");
    }

    /// 单飞：并发两次 run，探针只被轮询一次，两方拿到同一结论。
    #[tokio::test]
    async fn single_flight_shares_one_probe() {
        let runner = Arc::new(ProbeRunner::new());
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mk = |runner: Arc<ProbeRunner>, counter: Arc<std::sync::atomic::AtomicUsize>| async move {
            runner
                .run("k", async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    ProbeOutcome::Probed {
                        stage: ProbeStage::P3,
                        revision: Some(7),
                        duration_ms: 30,
                    }
                })
                .await
        };
        let (a, b) = tokio::join!(
            mk(runner.clone(), counter.clone()),
            mk(runner, counter.clone())
        );
        assert_eq!(a, b, "follower 必须拿到 leader 的同一结论");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "同一 key 同刻只跑一条探测"
        );
    }

    /// 单飞：不同 key 各自独立（不互相串用结论）。
    #[tokio::test]
    async fn single_flight_is_scoped_per_key() {
        let runner = Arc::new(ProbeRunner::new());
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mk = |key: &'static str,
                  runner: Arc<ProbeRunner>,
                  counter: Arc<std::sync::atomic::AtomicUsize>| async move {
            runner
                .run(key, async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ProbeOutcome::Inconclusive {
                        stage: ProbeStage::P1,
                        error_code: "no-client-found".into(),
                        duration_ms: 0,
                    }
                })
                .await
        };
        let _ = tokio::join!(
            mk("k1", runner.clone(), counter.clone()),
            mk("k2", runner, counter.clone())
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// 硬上限：探测挂死 → 15s 上限返回 inconclusive（用暂停时钟验证，不真等 15s）。
    #[tokio::test(start_paused = true)]
    async fn hard_cap_turns_hang_into_inconclusive() {
        let runner = ProbeRunner::new();
        let outcome = runner
            .run("k", async { std::future::pending::<ProbeOutcome>().await })
            .await;
        assert!(matches!(outcome, ProbeOutcome::Inconclusive { .. }));
        assert_eq!(outcome.error_code(), Some("probe_timeout"));
        assert_eq!(outcome.stage(), ProbeStage::P3);
        assert_eq!(outcome.duration_ms(), TOTAL_BUDGET.as_millis() as u64);
        assert_eq!(runner.inflight(), 0, "结束后不留在飞槽");
    }

    /// 结论 → 账本状态映射（verified 永不出现）。
    #[test]
    fn outcomes_map_to_ledger_states() {
        assert_eq!(
            ProbeOutcome::Probed {
                stage: ProbeStage::P3,
                revision: Some(1),
                duration_ms: 1
            }
            .as_state(),
            ProbeState::Probed
        );
        assert_eq!(
            ProbeOutcome::Failed {
                stage: ProbeStage::P0,
                error_code: "disconnected".into(),
                duration_ms: 1
            }
            .as_state(),
            ProbeState::Failed
        );
        assert_eq!(
            ProbeOutcome::Inconclusive {
                stage: ProbeStage::P2,
                error_code: "protocol_drift_suspected".into(),
                duration_ms: 1
            }
            .as_state(),
            ProbeState::Inconclusive
        );
    }
}

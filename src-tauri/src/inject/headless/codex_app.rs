//! P1 codex APP 协议面客户端：`codex app-server`（裸 spawn）stdio JSON-RPC——Task 5。
//!
//! # 定位（设计 §3.5/§3.4 + 计划「⚠️ Task 0 探测回填」定案，冲突以回填为准）
//!
//! 本模块是 codex APP 会话**写侧动作**（档位切换 / 中止）与**协议读回**（thread/read）的
//! 客户端本体；api.rs 接线归 Task 6a/6b，可用性探测归 Task 7（本任务不做缓存）。
//! 通道之间不互相依赖的纪律在本模块有一处**计划明令的例外**：thread UUID 映射复用
//! [`super::codex::resolve_thread`]（锚点 codex.rs:263-289，纯函数复用、零运行时耦合）。
//!
//! # 协议事实（全部活体实证，2026-10-12 二轮探测 + 一轮 §1；0.162.1 探测态）
//!
//! - **裸方言**：spawn `codex app-server`（裸）的 stdio JSONL 响应帧**无 `jsonrpc` 字段**
//!   （一轮 §1.1 / 二轮 §1.3）——解析按 `id`/`result`|`error`/`method` 识别，**不按 2.0
//!   严格校验**（字段在场也不拒，只是不依赖它）。通知旁流（无 id：`configWarning` /
//!   `remoteControl/status/changed` / `turn/*` 事件流）与响应帧**分道**（按有无 id）。
//! - **生命周期 = per-action 短命会话**（裁决 32a）：每动作 spawn → initialize → 调用 →
//!   读回 → 优雅退出。常驻池不做（三期 F3.3 边界）。**P2 扩展缝**：ServerRequest（id +
//!   method 的服务端请求，审批三族 + elicitation）是裁决 32 唯一持连 carve-out 的入口——
//!   本批只在会话上**收集不代答**（[`AppServerSession::server_requests`]）。
//! - **写路径仅对非活跃线程可用**（T0.1 决定性负结论）：APP 持有的活跃线程，裸实例与
//!   daemon 插座两腿的 resume 都被 `-32600 "already has an active writer"` 拒（锁 =
//!   codex-home 跨进程级、无 takeover 参数）——「写 APP 活跃会话」在 0.162.1 无任何通道。
//!   **daemon 通道子项已按 T0.1 删除**（对 APP 持有线程同样拒写，不值得 WS 依赖）：
//!   零 WebSocket、零 socket2 依赖；模块级语义 =
//!   [`ACTIVE_THREAD_WRITE_UNAVAILABLE`]（调用方据此降级，不假装可写）。
//! - **档位载体**（T0.2）：主载体① = `turn/start.approvalPolicy`（回合级生效后续沿用；
//!   input 必填 ⇒ 纯切档须附极小输入 = 一次回合成本）；兜底② = `thread/resume.approvalPolicy`
//!   （线程级一次 resume 长效；rollout `turn_context.approval_policy` 逐回合精确跟随）；
//!   ③ `config/value/write` **弃用**（untrusted 档被 0.162.1 拒写 "no longer supported" +
//!   全局副作用须备份回滚 + 仅影响新线程）——不留变体，志废于此。
//! - **读路径**：仅 `thread/read` 定点 by rollout UUID（裸实例 `thread/list` 恒 0 线程，
//!   不枚举磁盘线程）；元数据在 `result.thread`（33 键），**无 approvalPolicy** ⇒
//!   **档位读回不走 RPC**（T0.5：RPC 读回族全族不暴露档位；GET 臂真源 = rollout 尾部
//!   `turn_context.approval_policy`，接线归 Task 6a——本模块经 [`PolicyReadbackSeam`] 消费）。
//! - **interrupt**：`turn/interrupt {threadId, turnId}` 仅对 **MAM 自持连接的在飞回合**
//!   可用（二轮探测 §1.5：自持 31ms 生效 → interrupted）；对 APP 发起的回合 / 非自持回合
//!   会被写者锁拒 ⇒ 调用方先自判自持，非自持走 [`INTERRUPT_NOT_SELF_HELD_REASON`] 如实置灰。
//! - **优雅退出纪律**（二轮探测 §1.8）：MAM 自己持写者时**绝不硬杀**——关 stdin 等自然
//!   退出（干净退出即释放锁；硬杀会让写者锁滞留 ≤65s）。
//!
//! # 调用契约（质量 I1）
//!
//! **同步阻塞**：本客户端全程同步（std::process + 读线程 + `mpsc::recv_timeout`）——
//! 调用方必须放 `spawn_blocking`，**绝不在 async 运行时线程上直呼**（口径同
//! `wb_acp::run_turn` 的同步外壳；生产缝 [`production_spawn_seam`] 同此契约）。
//!
//! # 测试纪律（宪法级）
//!
//! 单测**绝不** spawn 真 codex / 不碰真实 `~/.codex`：进程出口全部经 [`SpawnSeam`] 缝注入
//! （tests 的脚本桩 [`MockIo`]），生产缝 [`production_spawn_seam`] 为 `cfg(not(test))`——
//! 测试构建零真实进程零网络；其验证面 = Task 7 探测 + M21/M22 真机验收。

use std::sync::Arc;

// 计划明令复用（锚点 codex.rs:263-289）：rollout 文件名 UUID 唯一来源 + 单写者锁特征串
// （跨通道纯函数复用，零运行时耦合；若后续上提到 turn.rs 由后续批次处理，勿在此复制实现）
use super::codex::{hits_writer_lock_marker, resolve_thread};
// Task 7 可用性探测：复用 gate 的缓存纪律（ProbeCache/ProbeKey/probe_cached——
// 平行 spec 结构，不扩 gate::ProbeSpec：探测体是交互式会话，不走 argv 单发探针族）
use super::gate::{probe_cached, ProbeCache, ProbeKey, ProbeVerdict};

/// `codex app-server` 子命令（裸 spawn = `<codex> app-server`）
pub const APP_SERVER_ARG: &str = "app-server";
/// JSON-RPC method not found（方法缺席 → unavailable 语义；Task 7 方法在场性探测同判据）
pub const METHOD_NOT_FOUND_CODE: i64 = -32601;
/// JSON-RPC invalid request（跨实例对活跃线程写 = -32600 + `already has an active writer`）
pub const INVALID_REQUEST_CODE: i64 = -32600;

/// 模块级语义旗标（T0.1 定案）：APP 持有的活跃线程写动作**结构性不可用**——
/// 裸实例与 daemon 两腿皆被单写者锁拒（协议层无 takeover 门）。审计/回执分类列用词。
pub const ACTIVE_THREAD_WRITE_UNAVAILABLE: &str = "active_thread_write_unavailable";
/// 活跃线程写不可用的用户面降级文案（Task 6a 打开态如实拒 / Task 8 ModeBarApp 灰置同口径）
pub const ACTIVE_THREAD_WRITE_REASON: &str = "会话正被 APP 使用，请关闭 APP 后重试或在 APP 内操作";
/// interrupt 对非 MAM 自持回合的如实不可用文案（Task 6b 消费——不在关闭态假装提供协议中止）
pub const INTERRUPT_NOT_SELF_HELD_REASON: &str = "活跃回合由 APP 持有写者，暂无协议中止通道";

/// 取消探针（Task 6b 中止臂接线）：回合持连侧在等待收口期轮询；`true` = 取消已请求
/// （cancel 端点经 turn 登记表的 armed 靶子置旗）。interrupt 必须由**持连侧**发出
/// （跨连接会被写者锁拒——T0.1 决定性负结论），本探针就是「持连侧感知取消」的缝。
pub type CancelProbe = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

// ============================================================
// 裸方言帧解析（纯核）
// ============================================================

/// 裸方言单帧分类（stdio JSONL 每行）。**不按 JSON-RPC 2.0 严格校验**：响应帧无
/// `jsonrpc` 字段（探测定案），分类只看 `id` / `result`|`error` / `method` 组合，
/// `jsonrpc` 字段在场也不拒（只是不依赖）。
#[derive(Debug, Clone, PartialEq)]
pub enum WireFrame {
    /// 响应帧（id + `result`|`error`）——请求-响应匹配的唯一来源
    Response { id: u64, payload: FramePayload },
    /// 服务端→客户端请求（id + method：ServerRequest 审批三族 + elicitation）。
    /// **P2 审批应答的扩展缝**（裁决 32 唯一持连 carve-out）：本批只收集不代答
    ServerRequest { id: u64, method: String },
    /// 通知旁流（无 id：`configWarning` / `remoteControl/status/changed` / `turn/*` 事件）
    Notification {
        method: String,
        params: serde_json::Value,
    },
    /// 未知行（非 JSON / 无已知键组合）——**只计数不解释**（纪律同 receipt 的未知帧）
    Unknown,
}

/// 响应帧载荷
#[derive(Debug, Clone, PartialEq)]
pub enum FramePayload {
    Ok(serde_json::Value),
    Err(FrameError),
}

/// 结构化协议错误（`error.code` / `error.message`；形状不符时整帧归未知，不猜）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    pub code: i64,
    pub message: String,
}

/// 裸方言单帧解析（纯核）。判序：① 有 `id`（u64）且有 `result` → 响应 Ok；
/// ② 有 `id` 且有结构化 `error`（code+message 双全才认，残缺归未知——不猜）→ 响应 Err；
/// ③ 有 `id` 且有 `method` → ServerRequest；④ 无 `id` 有 `method` → 通知旁流；
/// ⑤ 其余（非 JSON / 键组合不识）→ 未知行。`jsonrpc` 字段全程不看（在场不拒、缺席不怪）
pub fn parse_frame(line: &str) -> WireFrame {
    let v: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return WireFrame::Unknown,
    };
    let Some(obj) = v.as_object() else {
        return WireFrame::Unknown;
    };
    let id = obj.get("id").and_then(serde_json::Value::as_u64);
    let err = obj.get("error").and_then(|e| {
        // 结构化错误双键齐全才认（code 缺场/非数的错误形状归未知——探测只实证过标准形状）
        let code = e.get("code").and_then(serde_json::Value::as_i64)?;
        let message = e.get("message").and_then(serde_json::Value::as_str)?;
        Some(FrameError {
            code,
            message: message.to_string(),
        })
    });
    if let Some(id) = id {
        if let Some(result) = obj.get("result") {
            return WireFrame::Response {
                id,
                payload: FramePayload::Ok(result.clone()),
            };
        }
        if let Some(err) = err {
            return WireFrame::Response {
                id,
                payload: FramePayload::Err(err),
            };
        }
        if let Some(m) = obj.get("method").and_then(serde_json::Value::as_str) {
            return WireFrame::ServerRequest {
                id,
                method: m.to_string(),
            };
        }
        return WireFrame::Unknown; // 有 id 但无 result/error/method——不猜
    }
    if let Some(m) = obj.get("method").and_then(serde_json::Value::as_str) {
        return WireFrame::Notification {
            method: m.to_string(),
            params: obj
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        };
    }
    WireFrame::Unknown
}

// ============================================================
// 错误分类（调用方据此降级）
// ============================================================

/// 客户端动作错误。`wire_word()` 供审计/回执分类列（Task 6a/6b 消费）；
/// [`Display`] 面向用户（降级文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppServerError {
    /// 通道级失败：spawn 失败 / EOF 无响应 / initialize 失败 / 响应形状不可解析
    Channel(String),
    /// 目标不可定位（resolve_thread 无 UUID / threadId·turnId·input 为空）——
    /// **投递前拒绝**，零协议副作用
    InvalidTarget(String),
    /// APP 持有的活跃线程写动作结构性不可用（T0.1：两腿皆 -32600，无 takeover 门）——
    /// 跨实例写的实测形态 = `-32600` + `already has an active writer`
    ActiveThreadWriteUnavailable,
    /// 方法缺席（-32601 形态）→ unavailable（版本漂移的如实信号；非 panic 非成功）
    MethodUnavailable { method: String, message: String },
    /// 其余协议错误（code/message 原样透传）
    Rpc { code: i64, message: String },
}

impl AppServerError {
    /// 审计/回执分类词（单点；[`ACTIVE_THREAD_WRITE_UNAVAILABLE`] 为降级旗标词）
    pub const fn wire_word(&self) -> &'static str {
        match self {
            AppServerError::Channel(_) => "channel_error",
            AppServerError::InvalidTarget(_) => "invalid_target",
            AppServerError::ActiveThreadWriteUnavailable => ACTIVE_THREAD_WRITE_UNAVAILABLE,
            AppServerError::MethodUnavailable { .. } => "method_unavailable",
            AppServerError::Rpc { .. } => "rpc_error",
        }
    }
}

impl std::fmt::Display for AppServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppServerError::Channel(m) => write!(f, "协议通道不可用：{m}"),
            AppServerError::InvalidTarget(m) => write!(f, "目标不可用：{m}"),
            AppServerError::ActiveThreadWriteUnavailable => {
                write!(f, "{ACTIVE_THREAD_WRITE_REASON}")
            }
            AppServerError::MethodUnavailable { method, message } => {
                write!(f, "协议方法不可用（{method}）：{message}")
            }
            AppServerError::Rpc { code, message } => write!(f, "协议错误 {code}：{message}"),
        }
    }
}

/// 错误帧分类（纯核）：-32601 → 方法缺席（unavailable）；-32600 + 单写者锁特征串
/// （[`hits_writer_lock_marker`]，**大小写口径与 `codex.rs` 共用同一个单点**——评审 M3）→
/// 活跃线程写不可用；其余原样透传
fn classify(method: &str, e: FrameError) -> AppServerError {
    if e.code == METHOD_NOT_FOUND_CODE {
        return AppServerError::MethodUnavailable {
            method: method.to_string(),
            message: e.message,
        };
    }
    if e.code == INVALID_REQUEST_CODE && hits_writer_lock_marker(&e.message) {
        // 跨实例对活跃线程写的实测形态（T0.1 §1.4）：-32600 与 writer 语义同现才定性——
        // 裸 -32600 不当锁（别的 invalid request 不得被误降级），同纪律见 codex.rs 的锁判据
        return AppServerError::ActiveThreadWriteUnavailable;
    }
    AppServerError::Rpc {
        code: e.code,
        message: e.message,
    }
}

// ============================================================
// spawn 规格 + 执行缝（唯一进程出口）
// ============================================================

/// 裸实例 spawn 规格（argv 全量；构造见 [`bare_argv`]）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub argv: Vec<String>,
}

/// 裸 spawn argv 构造（纯核）：解析出的 exe 经 [`super::turn::spawn_shape`] 决定垫片包装
/// （Windows npm 垫片 `codex.cmd` 须 `cmd /c` 转一手，CreateProcess 不认批处理），再挂
/// `app-server` 子命令
pub fn bare_argv(exe: &str, os: &str) -> Vec<String> {
    let shape = super::turn::spawn_shape(exe, os);
    let mut argv = Vec::with_capacity(2 + shape.prefix.len());
    argv.push(shape.program);
    argv.extend(shape.prefix);
    argv.push(APP_SERVER_ARG.to_string());
    argv
}

/// 一枚裸实例会话的传输面（**执行缝**）。生产 = stdio 子进程
/// （[`production_spawn_seam`]，`cfg(not(test))`）；测试 = 脚本桩（tests 的 [`MockIo`]）。
pub trait AppServerIo: Send {
    /// 写一行（JSON 帧原文；实现自行补换行与 flush）
    fn send(&mut self, line: &str) -> Result<(), String>;
    /// 读下一行 stdout：`None` = EOF；`Some(Err)` = IO 错误/超时
    fn recv(&mut self) -> Option<Result<String, String>>;
    /// 优雅收尾：**关 stdin 让进程自然退出**（探测 §1.8：干净退出即释放写者锁；
    /// 硬杀会让锁滞留 ≤65s——kill 只能是宽限后的最后兜底）
    fn finish(&mut self);
}

/// spawn 缝（唯一进程出口；测试注入脚本桩，生产 [`production_spawn_seam`]）
pub type SpawnSeam = Arc<dyn Fn(&SpawnSpec) -> Result<Box<dyn AppServerIo>, String> + Send + Sync>;

/// initialize 的 clientInfo（探测定案 params 形状：`{"clientInfo":{"name","title","version"}}`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub name: String,
    pub title: String,
    pub version: String,
}

impl ClientInfo {
    /// MAM 生产身份（版本 = 包版本单点）
    pub fn mam() -> Self {
        Self {
            name: "mam".to_string(),
            title: "MAM Remote".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

// ============================================================
// 会话（spawn + initialize → 调用 → 退出）
// ============================================================

/// `thread/read` 元数据（T0.5 §3.1：数据在 `result.thread`，33 键节选有用面；
/// **无 approvalPolicy**——档位读回不走 RPC）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadMeta {
    pub id: Option<String>,
    /// `status.type`（探测：`notLoaded` = 无活跃写者载入——跨实例只读的常见态）
    pub status_type: Option<String>,
    /// `canAcceptDirectInput`
    pub can_accept_direct_input: Option<bool>,
    /// 会话宿主识别（`codex_work_desktop`+`vscode` = VS Code 桌面扩展；`codex-tui`+`cli` = CLI）
    pub originator: Option<String>,
    pub source: Option<String>,
}

impl ThreadMeta {
    /// 从 `result.thread` 防御性抽取（已知键缺场 = None，不猜）
    fn from_json(thread: &serde_json::Value) -> Self {
        let s = |k: &str| {
            thread
                .get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        Self {
            id: s("id"),
            status_type: thread
                .get("status")
                .and_then(|st| st.get("type"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            can_accept_direct_input: thread
                .get("canAcceptDirectInput")
                .and_then(serde_json::Value::as_bool),
            originator: s("originator"),
            source: s("source"),
        }
    }
}

/// `turn/start` 受理回执（响应形状双路防御：一轮 §1.3 `result.turn.status=inProgress`；
/// 二轮 §1.7 亦有 `turnId` 直挂 result 顶层——两处活体引用都在，取到哪个算哪个）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStartReceipt {
    pub turn_id: Option<String>,
    /// 受理态（探测：`inProgress`）
    pub status: Option<String>,
}

/// `turn/completed` 的 interrupted 终态词（探测 §1.5 活体；api.rs 切档回执的
/// `interrupted` 字段同源——Task 7 提取常量防魔法串漂移）
pub const TURN_STATUS_INTERRUPTED: &str = "interrupted";

/// `turn/completed` 通知的观测产物
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCompletion {
    /// `completed` / `interrupted` / `failed`（探测 §1.7 有 failed 态——模型层失败，非协议失败）
    pub status: Option<String>,
    pub duration_ms: Option<u64>,
    /// 是否观测到完成通知；EOF/IO 结束前未见 = false（如实「未观测到」，不猜成败）
    pub observed: bool,
}

/// `turn/completed` 通知参数 → 观测产物（**解析单点**：直接等待窗与 M22 留档补检同用）。
/// 形状按探测归档（§1.5：`params.turn.{status,durationMs}`）；顶层 `status` 作兼容双路；
/// 两路都缺 → status=None（观测到完成但形状漂移，不猜）。到达即 `observed=true`
/// （「到达」是事实，「成败」看 status——两码事）
fn turn_completion_from(params: &serde_json::Value) -> TurnCompletion {
    let turn = params.get("turn");
    let status = turn
        .and_then(|t| t.get("status"))
        .or_else(|| params.get("status"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let duration_ms = turn
        .and_then(|t| t.get("durationMs"))
        .and_then(serde_json::Value::as_u64);
    TurnCompletion {
        status,
        duration_ms,
        observed: true,
    }
}

/// `turn/interrupt` 回执（仅 MAM 自持连接的在飞回合——T0.1：自持 31ms 生效）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptReceipt {
    pub thread_id: String,
    pub turn_id: String,
    /// interrupt 响应里的 turn 态（在场则透出；缺席 = None 不猜）
    pub response_status: Option<String>,
}

/// 一枚裸实例会话（spawn + initialize 握手后的活连接）。
///
/// 生命周期归调用方（per-action 短命：起 → 调用 → [`AppServerSession::close`]）；
/// **P2 扩展缝**：`server_requests` 收集审批三族/elicitation 帧（裁决 32 唯一持连
/// carve-out 的入口）——本批只收集，代答归 P2（HeadlessApprovalCard 批）。
pub struct AppServerSession {
    io: Box<dyn AppServerIo>,
    next_id: u64,
    /// 通知旁流（无 id 帧的 method 序列——分道收集，绝不参与响应匹配）
    pub notifications: Vec<String>,
    /// **M22 吞帧竞窗的接住点**：`call()` 等 id 匹配响应期间先到的 `turn/completed`
    /// （interrupt 应答与完成帧竞态，见 [`Self::wait_turn_completed_cancellable`]）。
    /// 旁流只留方法名、参数已丢（复原不出完成态），故完成态在此**原样留档**待等待窗
    /// 认领——不静默吞掉「回合已完成」这一事实
    pending_turn_completion: Option<TurnCompletion>,
    /// ServerRequest 收集（P2 审批应答扩展缝；本批只收集不代答）
    pub server_requests: Vec<(u64, String)>,
    /// initialize 回包 `userAgent`（版本坐标**登记不硬编码**：0.162.1 为探测态）
    pub server_user_agent: Option<String>,
    /// 未知行计数（不解释）
    pub unknown_frames: usize,
    /// 不匹配本会话 id 的响应帧计数（旁记）
    pub stray_responses: usize,
}

impl AppServerSession {
    /// spawn + initialize 握手。**initialize 失败一律归 [`AppServerError::Channel`]**
    /// （spawn 失败 / EOF / 错误帧——握手都没成，谈不上方法语义）
    pub fn start(
        spawn: &SpawnSeam,
        spec: &SpawnSpec,
        client: &ClientInfo,
    ) -> Result<Self, AppServerError> {
        let io = spawn(spec).map_err(AppServerError::Channel)?;
        let mut s = Self {
            io,
            next_id: 1,
            notifications: Vec::new(),
            pending_turn_completion: None,
            server_requests: Vec::new(),
            server_user_agent: None,
            unknown_frames: 0,
            stray_responses: 0,
        };
        let params = serde_json::json!({
            "clientInfo": {
                "name": client.name,
                "title": client.title,
                "version": client.version,
            }
        });
        // initialize 失败一律通道面（EOF / 错误帧——握手没成，谈不上方法语义）
        let result = s.call("initialize", params).map_err(|e| {
            AppServerError::Channel(format!("ACP_INITIALIZE_FAIL: initialize 失败：{e}"))
        })?;
        s.server_user_agent = result
            .get("userAgent")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        Ok(s)
    }

    /// 请求-响应调用（裸方言：按 id 匹配响应帧；通知旁流/ServerRequest/未知行分道收集）
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AppServerError> {
        let id = self.next_id;
        self.next_id += 1;
        // 裸方言请求帧（探测 §1.1）：{"id":N,"method":"…","params":{…}}——同样不带 jsonrpc
        let frame = serde_json::json!({"id": id, "method": method, "params": params});
        let line = serde_json::to_string(&frame).map_err(|e| {
            AppServerError::Channel(format!("ACP_REQUEST_SERIALIZE_FAIL: 请求序列化失败：{e}"))
        })?;
        self.io.send(&line).map_err(AppServerError::Channel)?;
        loop {
            let line = match self.io.recv() {
                Some(Ok(l)) => l,
                Some(Err(e)) => {
                    return Err(AppServerError::Channel(format!(
                        "ACP_RESPONSE_READ_FAIL: {method}(id={id}) 读取失败：{e}"
                    )))
                }
                None => {
                    return Err(AppServerError::Channel(format!(
                        "ACP_EOF_BEFORE_RESPONSE: 连接在 {method}(id={id}) 响应前关闭（EOF）"
                    )))
                }
            };
            match parse_frame(&line) {
                WireFrame::Response { id: rid, payload } if rid == id => match payload {
                    FramePayload::Ok(v) => return Ok(v),
                    FramePayload::Err(e) => return Err(classify(method, e)),
                },
                // 他人 id 的响应帧：旁记计数（不匹配、不解释——同纪律的「不猜」面）
                WireFrame::Response { .. } => self.stray_responses += 1,
                WireFrame::Notification { method: m, params } => {
                    // 旁流照收（既有消费语义不变：方法名照旧进 `notifications`，不去重、
                    // 不由本层取走）；`turn/completed` 另**留档**完成态——M22 吞帧竞窗：
                    // 等响应期间先到的完成帧只留方法名的话，参数就丢了，外层等待窗此后
                    // 永远等不到第二帧 ⇒ 丢失「回合已完成」这一事实（observed 恒假）
                    if m == "turn/completed" {
                        self.pending_turn_completion = Some(turn_completion_from(&params));
                    }
                    self.notifications.push(m);
                }
                WireFrame::ServerRequest { id: rid, method: m } => {
                    self.server_requests.push((rid, m))
                }
                WireFrame::Unknown => self.unknown_frames += 1,
            }
        }
    }

    /// `thread/read` 定点读（T0.5 §3.1：元数据在 `result.thread`；跨实例只读已活体实证）
    pub fn read_thread_meta(&mut self, thread_id: &str) -> Result<ThreadMeta, AppServerError> {
        let result = self.call("thread/read", serde_json::json!({"threadId": thread_id}))?;
        let thread = result.get("thread").ok_or_else(|| {
            AppServerError::Channel(
                "CODEX_APP_THREAD_KEY_MISSING: thread/read 响应缺 thread 键（协议形状漂移，不猜）"
                    .to_string(),
            )
        })?;
        Ok(ThreadMeta::from_json(thread))
    }

    /// `thread/resume`（写路径第一步：装载线程；`approvalPolicy` 仅载体②时搭车——T0.2 兜底）
    fn resume_thread(
        &mut self,
        thread_id: &str,
        policy: Option<&AppApprovalPolicy>,
    ) -> Result<serde_json::Value, AppServerError> {
        let mut params = serde_json::Map::new();
        params.insert("threadId".to_string(), serde_json::json!(thread_id));
        if let Some(p) = policy {
            params.insert("approvalPolicy".to_string(), serde_json::json!(p.wire()));
        }
        self.call("thread/resume", serde_json::Value::Object(params))
        // resume 响应不回显档位（T0.2 载体②三腿一致实证）——核对只看 rollout 读回缝
    }

    /// `turn/start`（主载体①：`approvalPolicy` 搭车 + `input[]` 必填——纯切档附极小输入）
    fn start_turn(
        &mut self,
        thread_id: &str,
        input_text: &str,
        policy: Option<&AppApprovalPolicy>,
    ) -> Result<TurnStartReceipt, AppServerError> {
        let mut params = serde_json::Map::new();
        params.insert("threadId".to_string(), serde_json::json!(thread_id));
        // input 必填（schema 明示）：纯切档附极小输入——一次回合成本，T0.2 主载体已接受的代价
        params.insert(
            "input".to_string(),
            serde_json::json!([{ "type": "text", "text": input_text }]),
        );
        if let Some(p) = policy {
            params.insert("approvalPolicy".to_string(), serde_json::json!(p.wire()));
        }
        let result = self.call("turn/start", serde_json::Value::Object(params))?;
        // 响应形状双路防御（两处活体引用：一轮 §1.3 result.turn.status；二轮 §1.7 turnId 直挂）
        let turn = result.get("turn");
        let turn_id = result
            .get("turnId")
            .or_else(|| turn.and_then(|t| t.get("id")))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let status = turn
            .and_then(|t| t.get("status"))
            .or_else(|| result.get("status"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        Ok(TurnStartReceipt { turn_id, status })
    }

    /// 等待 `turn/completed` 通知（同连接自持等待窗 = interrupt 的作用窗）。
    /// EOF/IO 结束前未见 → `observed=false`（如实，不猜成败）。
    ///
    /// **等待上界（质量 M3）**：总等待预算归**调用方的 watchdog**；本方法不自带总死线，
    /// 只保证单帧读取不超过生产缝的 `PROC_READ_TIMEOUT_MS`（300s）预算。
    pub fn wait_turn_completed(&mut self) -> TurnCompletion {
        self.wait_turn_completed_cancellable(None, "", None)
    }

    /// [`Self::wait_turn_completed`] 的可中止形态（Task 6b 中止臂接线）：每帧之间轮询
    /// [`CancelProbe`]；首见 true 即在**本连接**上发 `turn/interrupt`（自持 31ms 生效
    /// ——T0.1 决定性负结论：跨连接 interrupt 会被写者锁拒，故必须由持连侧执行）后
    /// 继续等收口（完成帧的 `status=interrupted` 自然入 `TurnCompletion`）。interrupt
    /// 发送失败（版本漂移/通道错）不中止等待：回合自然收口、观测如实。turnId 缺席
    /// （形状漂移）= 无可中止对象 → 按未观测收口（不猜目标）。
    ///
    /// **M22 吞帧竞窗（质量 M5）**：`turn/completed` 可能**先于** interrupt 应答到达
    /// ——`interrupt()` 的 [`Self::call`] 只认 id 匹配的应答帧，期间先到的完成帧会落进
    /// 旁流留档（[`Self::pending_turn_completion`]）；本等待窗每轮先在留档位**补检**
    /// 一次，绝不在已吞帧的前提下继续 `recv()` 空等一帧永远不来的「第二帧」。
    pub fn wait_turn_completed_cancellable(
        &mut self,
        probe: Option<&CancelProbe>,
        thread_id: &str,
        turn_id: Option<&str>,
    ) -> TurnCompletion {
        let unobserved = TurnCompletion {
            status: None,
            duration_ms: None,
            observed: false,
        };
        let mut interrupt_sent = false;
        loop {
            // M22 补检（单点，先于探针与 recv）：call() 窗口吞下的完成帧在此认领。
            // 先于探针的理由：回合已收口 ⇒ 无在飞回合可中止，不必再发一次 interrupt
            if let Some(done) = self.pending_turn_completion.take() {
                return done;
            }
            if let (Some(probe), false) = (probe, interrupt_sent) {
                if probe() {
                    match turn_id.map(str::trim).filter(|t| !t.is_empty()) {
                        Some(tid) => {
                            // 发送失败不中止等待：回合自然收口，观测如实（结局由完成
                            // 态/读回判断，不在此假装已中止）
                            let _ = self.interrupt(thread_id, tid);
                            interrupt_sent = true;
                            // 立即回到补检：interrupt 应答等待窗内先到的完成帧已被
                            // 留档——落到下面的 recv() 就是 M22 的空等（永远等不到）
                            continue;
                        }
                        // 无 turnId = 无可中止对象（形状漂移不猜目标）——按未观测收口
                        None => return unobserved,
                    }
                }
            }
            let line = match self.io.recv() {
                Some(Ok(l)) => l,
                // IO 错误与 EOF 同面收口：等待结束且未见完成通知（如实 observed=false，不猜）
                Some(Err(_)) | None => return unobserved,
            };
            match parse_frame(&line) {
                WireFrame::Notification { method, params } if method == "turn/completed" => {
                    return turn_completion_from(&params);
                }
                WireFrame::Notification { method, .. } => self.notifications.push(method),
                WireFrame::ServerRequest { id: rid, method: m } => {
                    self.server_requests.push((rid, m))
                }
                WireFrame::Response { .. } => self.stray_responses += 1,
                WireFrame::Unknown => self.unknown_frames += 1,
            }
        }
    }

    /// `turn/interrupt`（**仅 MAM 自持连接的在飞回合**——T0.1：自持 31ms 生效；对 APP
    /// 发起/他人持有回合会被写者锁拒。调用方先自判自持，非自持走
    /// [`INTERRUPT_NOT_SELF_HELD_REASON`] 如实置灰，不在此假装可达）。
    ///
    /// **自持回合的构造（质量 M4）**：在飞回合须经**同一会话**的 resume + turn/start
    /// 起出（如 [`CodexAppClient::switch_approval_policy`] 的编排）；跨会话/跨实例打断
    /// 会被写者锁拒。「发送 + 等待 + 可中止」的公共复合入口归 Task 6b 接线，本模块不提供。
    pub fn interrupt(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<InterruptReceipt, AppServerError> {
        if thread_id.trim().is_empty() || turn_id.trim().is_empty() {
            return Err(AppServerError::InvalidTarget(
                "CODEX_APP_INTERRUPT_TARGET_REQUIRED: turn/interrupt 需要 threadId 与 turnId（空值投递前拒——不猜目标）"
                    .to_string(),
            ));
        }
        let result = self.call(
            "turn/interrupt",
            serde_json::json!({"threadId": thread_id, "turnId": turn_id}),
        )?;
        let response_status = result
            .get("turn")
            .and_then(|t| t.get("status"))
            .or_else(|| result.get("status"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        Ok(InterruptReceipt {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            response_status,
        })
    }

    /// 收尾（per-action 短命会话的退出步：优雅关 stdin，见 [`AppServerIo::finish`]）
    pub fn close(&mut self) {
        self.io.finish();
    }
}

// ============================================================
// 档位写（载体①/② + 读回三态核对）
// ============================================================

/// app-server 档位词（T0.2：三枚举参数层全支持 + 行为级已证；granular 结构化变体不在本批）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppApprovalPolicy {
    /// 从不请求批准（全自动档映射位）
    Never,
    /// 按需请求（默认档）
    OnRequest,
    /// 强制对非常信命令请求（安全档；**不可经 config 载体写**——0.162.1 拒写）
    Untrusted,
}

impl AppApprovalPolicy {
    /// wire 词单点（与 rollout `turn_context.approval_policy` 读回串同词表）
    pub const fn wire(self) -> &'static str {
        match self {
            AppApprovalPolicy::Never => "never",
            AppApprovalPolicy::OnRequest => "on-request",
            AppApprovalPolicy::Untrusted => "untrusted",
        }
    }

    /// 读回串 → 档（读回核对用；未知串 → None 不猜）。**宽容面（质量 M1）**：
    /// trim + ASCII 大小写不敏感——rollout 读回串带空白/大小写漂移不误报 Mismatch
    /// （wire 词本身恒小写；宽容只发生在解析侧，Mismatch 仍报原始串）
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim();
        ["never", "on-request", "untrusted"]
            .into_iter()
            .zip([
                AppApprovalPolicy::Never,
                AppApprovalPolicy::OnRequest,
                AppApprovalPolicy::Untrusted,
            ])
            .find(|(w, _)| t.eq_ignore_ascii_case(w))
            .map(|(_, p)| p)
    }

    /// 全部合法档（三枚举全量）。**GET /session-mode APP 面权限组 tiers 的数据源**
    /// （Task 7 补遗定案——与 current〔rollout 读回 wire 词〕同词表，交集闭合；
    /// 标签单点 = api.rs 的 codex_app_policy_label）。CLI 面 tiers 走 mode.rs 的
    /// CLI 四档表，两表分面不下沉本模块
    pub const ALL: [AppApprovalPolicy; 3] = [
        AppApprovalPolicy::Never,
        AppApprovalPolicy::OnRequest,
        AppApprovalPolicy::Untrusted,
    ];
}

/// 档位写载体（T0.2 定案兜底链 + 双审 I1 修订）：① [`PolicyCarrier::TurnStart`] 主载体
/// （回合级即时生效、后续沿用；行为级翻转实证）；② [`PolicyCarrier::ResumeOnly`] 兜底
/// **resume-only 形态**（review I1 修订：-32601 方法缺席的消费场景——只 resume 携档〔线程级
/// 一次 resume 长效持久〕、**不起 turn/start**：方法缺席还起回合 = 白付一次回合成本；
/// 本回合无新 turn_context ⇒ 核对恒 [`PolicyVerify::Ineffective`]，持久化语义由调用方
/// 回执注明）。[`PolicyCarrier::Resume`] 全形态（resume + 起回合）是 Task 5 钉值的
/// 能力面——生产兜底不走它（无消费者，留作能力文档与单测面）。载体③
/// `config/value/write` 弃用（见模块头）——不留变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyCarrier {
    /// ① `turn/start.approvalPolicy`（主载体；input 必填 ⇒ 附极小输入）
    TurnStart,
    /// ② `thread/resume.approvalPolicy` 全形态（resume 装载 + 起一回合；Task 5 钉值
    /// 能力面——生产兜底走 [`PolicyCarrier::ResumeOnly`]，本变体无生产消费者）
    Resume,
    /// ②' **resume-only 兜底**（review I1 修订）：只 `thread/resume {approvalPolicy}`
    /// （线程级持久、后续回合生效），不起 turn/start、无新 turn_context——核对恒
    /// `Ineffective`（不查读回缝：rollout 尾部仍是旧回合的 turn_context，比对会误报
    /// Mismatch），持久化语义在调用方 hint 注明
    ResumeOnly,
}

/// 档位读回缝（thread_id → rollout 尾部 `turn_context.approval_policy`；None = 读不到）。
/// **档位读回不走 RPC**（T0.5：RPC 读回族全族不暴露档位）——真源 = rollout 文件层，
/// 生产接线（monitor rollout 解析追加字段）归 Task 6a；本模块只消费它做三态核对。
pub type PolicyReadbackSeam = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// 档位写回执核对三态（设计 §3.6 协议读回三态——不猜）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyVerify {
    /// 读回与请求档一致
    Hit,
    /// 读回可读但为别档（如实报；前端不采信本地记忆、重拉 GET——Task 8 消费）
    Mismatch { readback: String },
    /// 读不到（rollout 无 `turn_context` / 不可读）→ 「未生效」如实（不谎报生效）
    Ineffective,
}

/// 档位写回执
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyWriteReceipt {
    pub carrier: PolicyCarrier,
    /// 请求档（wire 词）
    pub requested: String,
    /// 受理的回合 id（响应缺场 = None 不猜）
    pub turn_id: Option<String>,
    /// 回合收口观测（`turn/completed`；未观测到 = `observed=false`）
    pub turn_completion: TurnCompletion,
    /// 读回核对三态（**未观测到回合收口时恒 [`PolicyVerify::Ineffective`]**——质量 M4：
    /// 那时读回缝可能是**旧回合**上下文，比对会产出假 Mismatch）
    pub verify: PolicyVerify,
}

// ============================================================
// per-action 客户端（每动作 spawn → initialize → 调用 → 退出；裁决 32a）
// ============================================================

/// per-action 短命会话客户端：每个动作起一枚裸实例、完事即退。
///
/// **同步阻塞**：IO 缝是阻塞的，调用方必须放 `spawn_blocking`——绝不在 async 运行时
/// 线程上直呼（口径同 `wb_acp::run_turn`）。
pub struct CodexAppClient {
    spawn: SpawnSeam,
    spec: SpawnSpec,
    client: ClientInfo,
}

impl CodexAppClient {
    /// `spawn` = 执行缝（测试桩 / 生产 [`production_spawn_seam`]）；`spec` = spawn 规格
    pub fn new(spawn: SpawnSeam, spec: SpawnSpec, client: ClientInfo) -> Self {
        Self {
            spawn,
            spec,
            client,
        }
    }

    /// 起一枚短命会话（spawn + initialize）；调用方负责 `close`
    pub fn session(&self) -> Result<AppServerSession, AppServerError> {
        AppServerSession::start(&self.spawn, &self.spec, &self.client)
    }

    /// 读路径：`thread/read` 定点读。thread id 解析复用 [`super::codex::resolve_thread`]
    /// （rollout 文件名 UUID 权威 → UUID 形态会话号回退 → 都无则如实拒，不猜目标）
    pub fn read_thread(
        &self,
        rollout_path: Option<&str>,
        session_id: &str,
    ) -> Result<ThreadMeta, AppServerError> {
        let tref = resolve_thread(rollout_path, session_id).ok_or_else(|| {
            AppServerError::InvalidTarget(format!(
                "CODEX_APP_THREAD_ID_UNRESOLVED: 无法解析 codex thread id（rollout 路径与会话号都无 UUID）：{session_id}"
            ))
        })?;
        let mut s = self.session()?;
        let out = s.read_thread_meta(&tref.id);
        s.close();
        out
    }

    /// 写路径：档位切换（**仅非活跃线程**——活跃线程在 resume 步即 -32600 →
    /// [`AppServerError::ActiveThreadWriteUnavailable`]，调用方据此降级，不假装可写）。
    ///
    /// 编排：resume（载体②时档位搭车）→ turn/start（载体①时档位搭车 + 极小输入）→
    /// 等回合收口（`turn_context` 落盘后读回才可信）→ 读回三态核对（缝注入）。
    pub fn switch_approval_policy(
        &self,
        thread_id: &str,
        policy: AppApprovalPolicy,
        carrier: PolicyCarrier,
        input_text: &str,
        readback: &PolicyReadbackSeam,
    ) -> Result<PolicyWriteReceipt, AppServerError> {
        self.switch_approval_policy_cancellable(
            thread_id, policy, carrier, input_text, readback, None,
        )
    }

    /// [`Self::switch_approval_policy`] 的可中止形态（Task 6b）：等待收口窗轮询取消
    /// 探针——在飞回合可被 cancel 端点中止（持连侧自持 `turn/interrupt`）；其余步骤
    /// 语义不变。resume-only 兜底无等待窗（无回合 = 无作用对象），探针自然不消费
    pub fn switch_approval_policy_cancellable(
        &self,
        thread_id: &str,
        policy: AppApprovalPolicy,
        carrier: PolicyCarrier,
        input_text: &str,
        readback: &PolicyReadbackSeam,
        probe: Option<CancelProbe>,
    ) -> Result<PolicyWriteReceipt, AppServerError> {
        // 投递前拒（零 spawn 零回合——turn/start input 必填的诚实面，不烧 token）。
        // input 前置只约束「会起回合」的载体（① TurnStart / ② Resume 全形态）；
        // ②' ResumeOnly 不起回合 ⇒ 不要求 input（review I1：无回合就无必填可言）
        if thread_id.trim().is_empty() {
            return Err(AppServerError::InvalidTarget(
                "CODEX_APP_POLICY_THREAD_ID_REQUIRED: 档位写需要 threadId（空值投递前拒）"
                    .to_string(),
            ));
        }
        if carrier != PolicyCarrier::ResumeOnly && input_text.trim().is_empty() {
            return Err(AppServerError::InvalidTarget(
                "CODEX_APP_POLICY_INPUT_REQUIRED: 档位切换需附极小输入（协议回合 input 必填，会起回合的载体同规）"
                    .to_string(),
            ));
        }
        let mut s = self.session()?;
        let out = switch_on_session(
            &mut s,
            thread_id,
            policy,
            carrier,
            input_text,
            readback,
            probe.as_ref(),
        );
        s.close();
        out
    }
}

/// switch 的会话内主体（client 方法在 session/close 之间调用）
fn switch_on_session(
    s: &mut AppServerSession,
    thread_id: &str,
    policy: AppApprovalPolicy,
    carrier: PolicyCarrier,
    input_text: &str,
    readback: &PolicyReadbackSeam,
    probe: Option<&CancelProbe>,
) -> Result<PolicyWriteReceipt, AppServerError> {
    // ②' resume-only（review I1 兜底形态）：只 resume 携档（线程级持久、后续回合生效），
    // 不起 turn/start——本回合无新 turn_context ⇒ 核对恒 Ineffective（**不查读回缝**：
    // rollout 尾部仍是旧回合的 turn_context，比对会误报 Mismatch）；turn 无从观测
    // （observed=false 如实）。持久化语义由调用方 hint 注明
    if carrier == PolicyCarrier::ResumeOnly {
        s.resume_thread(thread_id, Some(&policy))?;
        return Ok(PolicyWriteReceipt {
            carrier,
            requested: policy.wire().to_string(),
            turn_id: None,
            turn_completion: TurnCompletion {
                status: None,
                duration_ms: None,
                observed: false,
            },
            verify: PolicyVerify::Ineffective,
        });
    }
    // 第一步 resume：装载线程（T0.2 兜底载体②时档位搭车；活跃线程在此步即被
    // -32600 拒 → ActiveThreadWriteUnavailable，调用方据此降级——不假装可写）
    let resume_policy = (carrier == PolicyCarrier::Resume).then_some(policy);
    s.resume_thread(thread_id, resume_policy.as_ref())?;
    // 第二步 turn/start：极小输入起回合（主载体①时档位搭车——回合级生效后续沿用）
    let turn_policy = (carrier == PolicyCarrier::TurnStart).then_some(policy);
    let started = s.start_turn(thread_id, input_text, turn_policy.as_ref())?;
    // 等回合收口：turn_context 落盘后读回才可信（T0.2 载体②持久层证据）；等待窗 =
    // interrupt 的作用窗（Task 6b：探针触发 → 持连侧自持 interrupt → 继续等收口）
    let turn_completion =
        s.wait_turn_completed_cancellable(probe, thread_id, started.turn_id.as_deref());
    // 读回三态核对（真源 = rollout 尾部 turn_context.approval_policy，缝注入——不走 RPC）
    let verify = if !turn_completion.observed {
        // **质量 M4：未观测到回合收口 ⇒ 降级 Ineffective**（不查读回缝）。判据：本回合
        // 的 `turn_context` 是随回合落盘的，`turn/completed` 未观测到意味着它未必已落盘
        // ——读回缝这时看到的可能仍是**旧回合**的上下文，拿它比对会产出**假 Mismatch**
        // （旧档 ≠ 请求档，前端据此判「不符」并丢弃乐观态）。三态里 Ineffective 的既有
        // 语义正是「读不到/未生效如实」（不谎报生效），口径与 ②' resume-only 分支的
        // 「不查读回缝」一致；真值仍可由调用方按 turn_completion 如实标注
        PolicyVerify::Ineffective
    } else {
        match readback(thread_id) {
            // 质量 M1：宽容判定——读回串经 parse（trim + 大小写不敏感）比对，漂移不误报
            // Mismatch；Mismatch.readback 仍报原始串（前端/审计看真值）
            Some(got) if AppApprovalPolicy::parse(&got) == Some(policy) => PolicyVerify::Hit,
            Some(got) => PolicyVerify::Mismatch { readback: got },
            None => PolicyVerify::Ineffective,
        }
    };
    Ok(PolicyWriteReceipt {
        carrier,
        requested: policy.wire().to_string(),
        turn_id: started.turn_id,
        turn_completion,
        verify,
    })
}

/// stderr 尾行环形缓冲（纯核）。**为什么必须 drain**：stderr piped-but-unread 会在
/// 子进程写满管道缓冲后死锁（spec 审 Major 1）；探测实证 stderr 有 hook 噪音可辅助
/// 诊断 ⇒ 保留诊断面 = drain 线程 + 尾行截断。上限口径对齐 runner.rs 的
/// `STDERR_TAIL_LINES = 8`（同款诊断面，勿另立数值）。
#[derive(Debug, Default)]
pub struct StderrTail {
    cap: usize,
    buf: std::collections::VecDeque<String>,
}

impl StderrTail {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            buf: std::collections::VecDeque::new(),
        }
    }

    /// 追加一行（超上限丢最旧行）
    pub fn push(&mut self, line: String) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(line);
    }

    /// 尾行拼接（诊断展示面；空缓冲 = 空串）
    pub fn joined(&self) -> String {
        self.buf.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

// ============================================================
// 生产出口（cfg(not(test))——测试构建零真实进程，验证面 = Task 7 探测 + M21/M22 真机）
// ============================================================

/// 生产读预算：单次 recv 等待上限（回合级等待——探测：最小回合 15–19s、审批面 +13.5s；
/// 300s 与既有 watchdog 默认同量级）
#[cfg(not(test))]
pub const PROC_READ_TIMEOUT_MS: u64 = 300_000;
/// 优雅退出宽限：关 stdin 后等自然退出的窗口；超窗才 kill 兜底（探测 §1.8：硬杀 →
/// 写者锁滞留 ≤65s——MAM 自持写者时 kill 是最后手段）
#[cfg(not(test))]
pub const FINISH_GRACE_MS: u64 = 5_000;
/// stderr 尾行缓冲上限（对齐 runner.rs 的 STDERR_TAIL_LINES = 8——同款诊断面，勿另立数值）
#[cfg(not(test))]
pub const STDERR_TAIL_LINES: usize = 8;

/// PATH 解析 codex exe（`.exe` 优先、垫片照认；os 运行期取值）——[`production_spec`]
/// 的解析单点，也是 Task 7 探测缓存键的 exe 来源（键用**垫片包装前**的解析路径，
/// 不用 `SpawnSpec.argv[0]`——`.cmd` 垫片的 argv[0] 是 `cmd`，会撞键）
#[cfg(not(test))]
pub fn production_exe(path_env: &str, os: &str) -> Option<String> {
    super::turn::cli_in_path("codex", path_env, os)
}

/// 生产 spawn 规格（PATH 解析 codex；os 运行期取值）
#[cfg(not(test))]
pub fn production_spec(path_env: &str, os: &str) -> Option<SpawnSpec> {
    production_exe(path_env, os).map(|exe| SpawnSpec {
        argv: bare_argv(&exe, os),
    })
}

/// 生产 spawn 缝：stdio 子进程——stdin/stdout 管道承载协议流量；**stderr 也走管道但由
/// drain 线程取尾行**（spec 审 Major 1：piped-but-unread 会在子进程写满管道缓冲后死锁；
/// 探测实证 stderr 有 hook 噪音可辅助诊断 ⇒ 不丢诊断面，照 runner.rs 的 STDERR_TAIL_LINES
/// 同款收尾行）。Windows 上 `CREATE_NO_WINDOW` 防控制台闪窗，仓内先例 monitor/git.rs）。
/// **`cfg(not(test))`**：测试构建本函数不存在 ⇒ 单测无法触达真实进程（宪法级纪律）。
#[cfg(not(test))]
pub fn production_spawn_seam() -> SpawnSeam {
    Arc::new(|spec: &SpawnSpec| {
        let program = spec
            .argv
            .first()
            .ok_or_else(|| "spawn 规格缺 argv[0]".to_string())?;
        let mut cmd = std::process::Command::new(program);
        cmd.args(&spec.argv[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Windows 下 GUI 进程 spawn 控制台程序会闪黑窗，必须 CREATE_NO_WINDOW
        // （仓内先例 monitor/git.rs 同款）
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("ACP_SPAWN_FAIL: spawn {program} 失败：{e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "stdin 管道不可用".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "stdout 管道不可用".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "stderr 管道不可用".to_string())?;
        ProcIo::new(program.to_string(), child, stdin, stdout, stderr)
            .map(|io| Box::new(io) as Box<dyn AppServerIo>)
    })
}

/// 生产传输面：stdout 读线程 + stderr drain 线程 + 超时预算 + 优雅收尾
/// （实现细节，只被 [`production_spawn_seam`] 构造）
#[cfg(not(test))]
struct ProcIo {
    program: String,
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    rx: std::sync::mpsc::Receiver<Result<String, String>>,
    stderr_tail: Arc<std::sync::Mutex<StderrTail>>,
    finished: bool,
}

#[cfg(not(test))]
impl ProcIo {
    fn new(
        program: String,
        child: std::process::Child,
        stdin: std::process::ChildStdin,
        stdout: std::process::ChildStdout,
        stderr: std::process::ChildStderr,
    ) -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("codex-app-io".to_string())
            .spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(Ok(l)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ =
                                tx.send(Err(format!("ACP_STDOUT_READ_FAIL: 读 stdout 失败：{e}")));
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("ACP_READ_THREAD_SPAWN_FAIL: 起读线程失败：{e}"))?;
        // stderr drain 线程（spec 审 Major 1）：不读会死锁，读了不截断会无界——
        // 环形缓冲只留尾行（诊断面保留）
        let stderr_tail = Arc::new(std::sync::Mutex::new(StderrTail::with_cap(
            STDERR_TAIL_LINES,
        )));
        let tail_for_thread = Arc::clone(&stderr_tail);
        std::thread::Builder::new()
            .name("codex-app-stderr".to_string())
            .spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stderr).lines() {
                    match line {
                        Ok(l) => tail_for_thread
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(l),
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| format!("ACP_STDERR_DRAIN_SPAWN_FAIL: 起 stderr drain 线程失败：{e}"))?;
        Ok(Self {
            program,
            child,
            stdin: Some(stdin),
            rx,
            stderr_tail,
            finished: false,
        })
    }
}

#[cfg(not(test))]
impl AppServerIo for ProcIo {
    fn send(&mut self, line: &str) -> Result<(), String> {
        use std::io::Write;
        let stdin = self.stdin.as_mut().ok_or("stdin 已关闭")?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("HEADLESS_STDIN_WRITE_FAIL: 写 stdin 失败：{e}"))
    }

    fn recv(&mut self) -> Option<Result<String, String>> {
        use std::sync::mpsc::RecvTimeoutError;
        if self.finished {
            return None;
        }
        match self
            .rx
            .recv_timeout(std::time::Duration::from_millis(PROC_READ_TIMEOUT_MS))
        {
            Ok(r) => Some(r),
            // 进程 stdout 关闭（自然退出路径）= EOF
            Err(RecvTimeoutError::Disconnected) => None,
            // 超时 ≠ EOF：如实报超时（调用方按通道错误面处理，不冒充 EOF）
            Err(RecvTimeoutError::Timeout) => Some(Err(format!(
                "ACP_READ_TIMEOUT: app-server {}ms 无输出（读取超时）",
                PROC_READ_TIMEOUT_MS
            ))),
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.stdin.take(); // drop 关闭 stdin → 进程自然退出（探测 §1.8 优雅路径）
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(FINISH_GRACE_MS);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                // 宽限尽仍未退：kill 兜底（登记：写者锁可能滞留 ≤65s——最后手段）。
                // 质量 M2：必须留痕（exe + 等待毫秒数）——静默 kill 会让「锁滞留」不可追溯
                Ok(None) => {
                    log::warn!(
                        "codex app-server（{}）优雅退出宽限 {}ms 已尽仍未退出——kill 兜底\
                         （写者锁可能滞留 ≤65s，探测 §1.8）；stderr 尾行：{}",
                        self.program,
                        FINISH_GRACE_MS,
                        self.stderr_tail
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .joined()
                    );
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
                Err(_) => return,
            }
        }
    }
}

#[cfg(not(test))]
impl Drop for ProcIo {
    fn drop(&mut self) {
        self.finish(); // 调用方忘 close 也优雅收尾（防进程泄漏）
    }
}

// ============================================================
// 端点依赖束 + 档位读回单点（Task 6a：api.rs 模式族协议臂的执行缝）
// ============================================================

/// api.rs 协议臂（`session_mode_switch` 的 codex APP 臂）的执行依赖束。
/// 生产 = 真 spawn（[`production_spawn_seam`]）+ rollout 档位读回；测试构建 =
/// [`test_hooks`] 脚本桩（零真实进程、零真实 ~/.codex——宪法级测试纪律的端点面
/// 延伸，形态沿 wb_acp / codex 的 test_hooks 先例）。
#[derive(Clone)]
pub struct AppArmDeps {
    /// spawn 规格（None = codex CLI 不在 PATH——端点如实拒「协议面不可用」，不裸 spawn）
    pub spec: Option<SpawnSpec>,
    /// spawn 缝（唯一进程出口）
    pub spawn: SpawnSeam,
    /// 档位读回缝（thread id → rollout 尾部 `turn_context.approval_policy`）
    pub readback: PolicyReadbackSeam,
}

/// **档位读回单点**（thread id → rollout 尾部 `turn_context.approval_policy`）：
/// GET /session-mode APP 臂的 `current` 与 POST 切档的核对三态**共此一真源**
/// （T0.5 定案：RPC 读回族全族不暴露 approvalPolicy，唯一可靠真源 = rollout
/// 文件层「最后一条 turn_context」——T0.2 载体②已证其逐回合精确跟随）。
/// 线索发现单点 = [`super::codex::production_rollout_path`]（state DB 的
/// threads.rollout_path；codex APP 会话号即 thread id）。**同步阻塞**
/// （SQLite 只读 + 尾读）——调用方放 spawn_blocking。
///
/// **测试构建经 [`test_hooks`] 注入**（零真实 ~/.codex；缺省 None = 读不到的
/// 保守降级形态，与生产缺位同向）。
pub fn policy_readback(home: Option<&std::path::Path>, thread_id: &str) -> Option<String> {
    #[cfg(test)]
    {
        let _ = (home, thread_id);
        test_hooks::readback()
    }
    #[cfg(not(test))]
    {
        let path = super::codex::production_rollout_path(home, thread_id)?;
        crate::monitor::codex_parser::rollout_tail_approval_policy(std::path::Path::new(&path))
    }
}

/// 生产/测试装配单点（Task 6a）：POST 协议臂拿完整依赖束；GET 臂经
/// [`policy_readback`] 间接消费读回缝（无需 spawn）。
pub fn app_arm_deps(home: Option<&std::path::Path>) -> AppArmDeps {
    #[cfg(test)]
    {
        let _ = home;
        AppArmDeps {
            spec: test_hooks::spec(),
            spawn: test_hooks::spawn_seam(),
            readback: test_hooks::readback_seam(),
        }
    }
    #[cfg(not(test))]
    {
        let seam_home = home.map(std::path::PathBuf::from);
        AppArmDeps {
            spec: production_spec(
                &std::env::var("PATH").unwrap_or_default(),
                std::env::consts::OS,
            ),
            spawn: production_spawn_seam(),
            readback: Arc::new(move |tid: &str| policy_readback(seam_home.as_deref(), tid)),
        }
    }
}

// ============================================================
// Task 7：app-server 可用性探测（设计 §3.5 版本门行 + §10 风险 2）
// ============================================================

/// 方法在场性探测集（T0.2/T0.5 定案方法面）：读回（`thread/read`）、档位写主载体①
/// （`turn/start`）与兜底②（`thread/resume`）、中止（`turn/interrupt`）。schema 317
/// 文件仅素材——**运行时以探测为准**（设计 §3.5）
pub const PROBE_PRESENCE_METHODS: [&str; 4] = [
    "thread/read",
    "thread/resume",
    "turn/start",
    "turn/interrupt",
];

/// 探测降级文案前缀（计划 Task 7 钉句「协议面不可用：<原因>」）。**spec 审 m-1
/// 收口**：本前缀只在 api.rs 的组合点（`probe_degraded_reason`）统一添加——GET 载荷
/// `degraded` / POST 臂 error / cancel 臂 reason 三路口径一致；探测结论的 hint 串
/// **不内嵌本前缀**（防「协议面不可用：……——协议面不可用」双前缀）
pub const PROBE_UNAVAILABLE_PREFIX: &str = "协议面不可用";

/// 探测缓存（进程级；键 = codex exe 在场性 + mtime——版本漂移自动失效重探）。
/// **独立于 `codex::probe_cache`**（CLI queue 探针）：两者 exe 同源，而
/// [`ProbeCache`] 以 exe 串为格键——共用实例会互相覆盖结论
static PROBE_CACHE: std::sync::LazyLock<ProbeCache> = std::sync::LazyLock::new(ProbeCache::default);

/// app-server 探测缓存单点（api.rs 接线消费）
pub fn probe_cache() -> &'static ProbeCache {
    &PROBE_CACHE
}

/// 探测核心（纯缝参数化——单测直调：本地 cache + mock seam，零真实进程零网络）：
/// spawn 裸实例 + initialize + [`PROBE_PRESENCE_METHODS`] 逐方法空参 ping。
///
/// # 方法在场性判据（零模型成本）
/// ping 帧的 `params` 恒空对象——`turn/start` 无 threadId/input 必被参数面拒，
/// 探针**绝不起真实回合**。应答分类只有缺席一罪：
/// - `-32601`（method not found）→ 方法缺席 → 探测 fail（版本漂移的如实信号）；
/// - **其余一切应答 = 在场**：`Ok` / `-32602` 参数面错 / **`-32600` + 单写者锁特征串**
///   （writer 锁是方法分派后的语义拒，恰是在场证据——与 -32601 必须分清）/ 其他
///   Rpc 错；通道面错误（EOF/IO/spawn/initialize 失败）→ fail。
///
/// **缓存纪律与 zcode/codex CLI 探针同款**（[`probe_cached`]）：成功长缓存 / 失败
/// 5min TTL（[`gate::FAILURE_TTL_MS`]）/ 键含 exe mtime（codex 升级即失效重探——
/// 防版本漂移时每动作都付一次失败 spawn，设计 §3.5 版本门行的初衷）。
/// **同步阻塞**（spawn + 会话 IO）——调用方放 `spawn_blocking`。
///
/// # 等待预算（spec 审 m-2 登记）
/// 探测体**不自带独立超时**——单帧等待继承生产读超时 `PROC_READ_TIMEOUT_MS`
/// （300s，生产传输面 ProcIo 的 recv 上限；测试构建该常量不在场，mock 缝无等待）。
/// 探测帧面小（initialize + 4 ping），最坏形态 = 一帧 300s 后通道错误如实 fail；
/// 失败结论经 5min TTL 缓频（坏通道每 TTL 至多重付一次探测，非每动作一付）。
/// **更紧的独立探测预算留后续批次**（如 10–15s 量级的探针专属死线——登记 Task 11
/// 遗留面，本批不动）。
pub fn probe_app_server(
    exe: &str,
    spawn: &SpawnSeam,
    spec: &SpawnSpec,
    client: &ClientInfo,
    cache: &ProbeCache,
) -> ProbeVerdict {
    let key = ProbeKey::of_exe(exe);
    probe_cached(cache, key, || {
        let started = std::time::Instant::now();
        probe_body(spawn, spec, client).with_duration_ms(started.elapsed().as_millis() as u64)
    })
}

/// 探测执行体（裸逻辑，不进缓存判定——只被 [`probe_app_server`] 的执行闭包调用）
fn probe_body(spawn: &SpawnSeam, spec: &SpawnSpec, client: &ClientInfo) -> ProbeVerdict {
    // spawn + initialize：握手失败一律通道面（spawn 失败 / EOF / 错误帧——握手没成
    // 谈不上方法语义，与 [`AppServerSession::start`] 的归类同口径）
    let mut s = match AppServerSession::start(spawn, spec, client) {
        Ok(s) => s,
        Err(AppServerError::Channel(m)) => {
            return ProbeVerdict::fail(format!(
                "ACP_CHANNEL_UNAVAILABLE_INIT: app-server 通道不可用（spawn/initialize）：{m}"
            ))
        }
        Err(e) => {
            return ProbeVerdict::fail(format!(
                "ACP_CHANNEL_UNAVAILABLE: app-server 通道不可用：{e}"
            ))
        }
    };
    for method in PROBE_PRESENCE_METHODS {
        match s.call(method, serde_json::json!({})) {
            // -32601 = 方法缺席（版本漂移）——fail + 缺席方法名进原因；短路不再 ping
            Err(AppServerError::MethodUnavailable { message, .. }) => {
                s.close();
                // hint 不带降级前缀（组合点统一加——见 PROBE_UNAVAILABLE_PREFIX doc）
                return ProbeVerdict::fail(format!(
                    "ACP_METHOD_MISSING: app-server 方法缺席：{method}（{message}）——版本漂移"
                ));
            }
            // 通道面错误（EOF/IO）：探测中断，如实 fail
            Err(AppServerError::Channel(m)) => {
                s.close();
                return ProbeVerdict::fail(format!(
                    "ACP_PROBE_INTERRUPTED: app-server 探测中断（{method} 通道错误）：{m}"
                ));
            }
            // 其余一切（Ok / -32600 writer 锁 / -32602 参数面 / 其他 Rpc）= 在场：
            // 方法已分派，应答是语义层的；空参 ping 无协议副作用（零模型成本）
            Ok(_) | Err(_) => {}
        }
    }
    let ua = s.server_user_agent.clone();
    s.close();
    let ver = ua
        .as_deref()
        .map(|u| format!("ACP_SERVER_VERSION_SUFFIX: ；服务端 {u}"))
        .unwrap_or_default();
    ProbeVerdict::pass(format!(
        "ACP_PROBE_PASS: app-server 探测通过（initialize + {} 方法在场{ver}）",
        PROBE_PRESENCE_METHODS.len()
    ))
}

/// 端点侧探测入口（Task 7 接线——api.rs 的 POST 切档臂 / GET 载荷 `degraded` /
/// cancel interrupt 臂前置）：PATH 解析 codex → [`probe_app_server`] + 进程级缓存
/// [`probe_cache`]；不在 PATH → 如实 fail（零 spawn）。**同步阻塞**——调用方放
/// `spawn_blocking`。
pub fn probe_app_server_arm(cache: &ProbeCache) -> ProbeVerdict {
    #[cfg(test)]
    {
        // 端点级测试钩子（零真实进程）：缺省 = **pass**——既有 6a/6b 端点用例的脚本面
        // 只描述协议回合，探测作为新前置层不侵入其脚本；fail 专项用例经
        // `test_hooks::set_probe` 显式装填。核心路径（缓存纪律/方法判定）由单测经
        // [`probe_app_server`] 直测，不经本钩子
        let _ = cache;
        test_hooks::probe_verdict()
    }
    #[cfg(not(test))]
    {
        let os = std::env::consts::OS;
        let path_env = std::env::var("PATH").unwrap_or_default();
        let Some(exe) = production_exe(&path_env, os) else {
            // hint 不带降级前缀（组合点统一加——见 PROBE_UNAVAILABLE_PREFIX doc）
            return ProbeVerdict::fail("codex CLI 不可达（PATH 上未找到 codex）".to_string());
        };
        let spec = SpawnSpec {
            argv: bare_argv(&exe, os),
        };
        probe_app_server(
            &exe,
            &production_spawn_seam(),
            &spec,
            &ClientInfo::mam(),
            cache,
        )
    }
}

/// 端点级测试钩子（仅 `cfg(test)`）：server.rs 的端点用例经 [`app_arm_deps`] /
/// [`policy_readback`] 取到这里的桩——**零真实进程、零真实 ~/.codex**。
///
/// **并行纪律**（与 codex / wb_acp 的 test_hooks 同款）：凡装填/断言钩子的用例
/// 必须持 [`LOCK`]（tokio 互斥量——用例是 `#[tokio::test]`，守卫必然跨 await 持有，
/// std 守卫会被 `clippy::await_holding_lock` 正当拦下）；用例收尾调 [`clear`]
/// （Drop 守卫模式亦可）。**缺省（未装填）**：spec = None / 脚本空（spawn 即失败）/
/// readback = None——「未注入 = 如实降级」，与生产缺位形态同向。
#[cfg(test)]
pub mod test_hooks {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 钩子用例串行锁（持锁期间其它端点用例不得断言协议臂行为）
    pub(crate) static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    static SPEC: std::sync::LazyLock<Mutex<Option<SpawnSpec>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    /// 脚本桩队列：每 spawn 弹一队（一队 = 一枚实例的全部应答行）；耗尽 → spawn 失败
    static SCRIPTS: std::sync::LazyLock<Mutex<VecDeque<Vec<String>>>> =
        std::sync::LazyLock::new(|| Mutex::new(VecDeque::new()));
    /// Task 7 探测门钩子（端点级）：缺省 None = **pass**（探测作为新前置层不侵入
    /// 既有端点用例的脚本面）；fail 专项用例经 [`set_probe`] 显式装填。
    /// 装填/断言须持 [`LOCK`]（与既有钩子同纪律——防并行用例互污）
    static PROBE: std::sync::LazyLock<Mutex<Option<ProbeVerdict>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    /// 读回值（对任意 thread id 恒定——端点用例只关心「读到/读不到」两态）
    static READBACK: std::sync::LazyLock<Mutex<Option<String>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    /// 每实例发出的帧（按 spawn 序——端点用例断言 turn/start 参数用）
    static SENT: std::sync::LazyLock<Mutex<Vec<Vec<String>>>> =
        std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

    /// 装填钩子（**调用方必须已持 [`LOCK`]**；`spec = None` 可模拟 codex 不可达）
    pub(crate) fn set(
        spec: Option<SpawnSpec>,
        scripts: Vec<Vec<String>>,
        readback: Option<String>,
    ) {
        *SPEC.lock().unwrap_or_else(|e| e.into_inner()) = spec;
        *SCRIPTS.lock().unwrap_or_else(|e| e.into_inner()) = scripts.into();
        *READBACK.lock().unwrap_or_else(|e| e.into_inner()) = readback;
        SENT.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 清空钩子（用例收尾铁律——防串扰下一个用例）
    pub(crate) fn clear() {
        *SPEC.lock().unwrap_or_else(|e| e.into_inner()) = None;
        SCRIPTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *READBACK.lock().unwrap_or_else(|e| e.into_inner()) = None;
        SENT.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 装填探测结论（Task 7；**调用方必须已持 [`LOCK`]**；None = 恢复缺省 pass）
    pub(crate) fn set_probe(verdict: Option<ProbeVerdict>) {
        *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = verdict;
    }

    /// 取探测结论（缺省 = pass——「未注入 = 门控通过」，见 PROBE 文档）
    pub(crate) fn probe_verdict() -> ProbeVerdict {
        PROBE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| ProbeVerdict::pass("测试缺省：app-server 探测通过"))
    }

    /// 各实例发出的帧（按 spawn 序；每实例一个 Vec，帧为 JSON 原文）
    pub(crate) fn sent_frames() -> Vec<Vec<String>> {
        SENT.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn readback() -> Option<String> {
        READBACK.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn spec() -> Option<SpawnSpec> {
        SPEC.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn readback_seam() -> PolicyReadbackSeam {
        Arc::new(|_tid: &str| readback())
    }

    pub(crate) fn spawn_seam() -> SpawnSeam {
        Arc::new(|_spec: &SpawnSpec| {
            let script = SCRIPTS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .ok_or_else(|| "test_hooks：脚本耗尽（未预期的额外 spawn）".to_string())?;
            Ok(Box::new(HookIo {
                script: VecDeque::from(script),
                sent: Vec::new(),
                finished: false,
            }) as Box<dyn AppServerIo>)
        })
    }

    /// 脚本桩 IO：recv 逐行弹脚本、send 记帧；finish 提交帧集（与单测 MockIo 的
    /// M5 守卫同款：finish 时脚本仍有剩余行 → Drop panic——脚本期望与实现行为
    /// 不符要当场炸出来，不留暗坑）
    struct HookIo {
        script: VecDeque<String>,
        sent: Vec<String>,
        finished: bool,
    }

    impl AppServerIo for HookIo {
        fn send(&mut self, line: &str) -> Result<(), String> {
            self.sent.push(line.to_string());
            Ok(())
        }

        fn recv(&mut self) -> Option<Result<String, String>> {
            self.script.pop_front().map(Ok)
        }

        fn finish(&mut self) {
            self.finished = true;
            let frames = std::mem::take(&mut self.sent);
            SENT.lock().unwrap_or_else(|e| e.into_inner()).push(frames);
        }
    }

    impl Drop for HookIo {
        fn drop(&mut self) {
            if !self.finished {
                // 调用方忘 close：仍提交帧集（不丢断言面），再走剩余行守卫
                self.finish();
            }
            if !self.script.is_empty() {
                panic!(
                    "test_hooks HookIo：finish 时脚本仍剩余 {} 行未消费（脚本期望与实现行为不符）",
                    self.script.len()
                );
            }
        }
    }
}

// ============================================================
// tests（纯核：全部经缝 mock，零真实进程零网络——宪法级纪律）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 真实形态的 thread UUID（一轮探测 §1.4 的目标线程，探测态样本）
    const TID: &str = "01a12b82-3ff5-7050-8e05-0af20b11ff87";
    /// 二轮探测 §1.7 的磁盘非活跃线程（rollout 文件名 UUID 权威源的夹具形态）
    const OLD_UUID: &str = "01a0e0b2-1111-4222-8333-444455556666";
    const ROLLOUT: &str = "C:/Users/x/.codex/sessions/2026/09/27/rollout-2026-09-27T12-00-00-01a0e0b2-1111-4222-8333-444455556666.jsonl";
    /// initialize 成功回包（探测 §1.3 活体形状：无 jsonrpc 字段；userAgent = 版本坐标）
    const INIT_RESULT: &str =
        "{\"userAgent\":\"codex-tui/0.162.1 (Windows 10.0.26200; x86_64)\",\"codexHome\":\"C:/Users/x/.codex\",\"platformFamily\":\"windows\",\"platformOs\":\"windows\"}";

    // ---- 帧构造夹具（裸方言原文）----

    fn resp(id: u64, result: &str) -> String {
        format!("{{\"id\":{id},\"result\":{result}}}")
    }

    fn resp_err(id: u64, code: i64, message: &str) -> String {
        format!("{{\"id\":{id},\"error\":{{\"code\":{code},\"message\":\"{message}\"}}}}")
    }

    fn note(method: &str) -> String {
        format!("{{\"method\":\"{method}\",\"params\":{{}}}}")
    }

    fn server_req(id: u64, method: &str) -> String {
        format!("{{\"id\":{id},\"method\":\"{method}\",\"params\":{{}}}}")
    }

    fn turn_completed(status: &str) -> String {
        format!(
            "{{\"method\":\"turn/completed\",\"params\":{{\"threadId\":\"{TID}\",\
             \"turn\":{{\"id\":\"turn-1\",\"status\":\"{status}\",\"durationMs\":6032}}}}}}"
        )
    }

    fn init_ok() -> String {
        resp(1, INIT_RESULT)
    }

    fn spec() -> SpawnSpec {
        SpawnSpec {
            argv: vec!["codex".to_string(), "app-server".to_string()],
        }
    }

    fn client_info() -> ClientInfo {
        ClientInfo {
            name: "mam-test".to_string(),
            title: "t".to_string(),
            version: "0".to_string(),
        }
    }

    fn client_of(seam: SpawnSeam) -> CodexAppClient {
        CodexAppClient::new(seam, spec(), client_info())
    }

    fn readback_of(v: Option<&str>) -> PolicyReadbackSeam {
        let v = v.map(str::to_string);
        Arc::new(move |_tid: &str| v.clone())
    }

    // ---- MockIo / 记录型 spawn 缝（零进程零网络）----

    #[derive(Default, Debug)]
    struct SpawnLog {
        specs: Vec<SpawnSpec>,
        /// 每实例：发出的帧 + 是否被显式 finish
        instances: Vec<InstanceLog>,
    }

    #[derive(Default, Debug)]
    struct InstanceLog {
        sent: Vec<String>,
        finished: bool,
    }

    struct MockIo {
        script: VecDeque<String>,
        sent: Vec<String>,
        finished: bool,
        committed: bool,
        log: Arc<Mutex<SpawnLog>>,
        /// finish 时仍未消费的脚本行数（M5 守卫：Drop 时 panic——脚本期望与实现行为
        /// 不符要当场炸出来，不留「脚本多写了几行也没人发现」的暗坑）
        leftover_at_finish: usize,
    }

    impl MockIo {
        fn commit(&mut self) {
            if self.committed {
                return;
            }
            self.committed = true;
            self.log.lock().unwrap().instances.push(InstanceLog {
                sent: std::mem::take(&mut self.sent),
                finished: self.finished,
            });
        }
    }

    impl AppServerIo for MockIo {
        fn send(&mut self, line: &str) -> Result<(), String> {
            self.sent.push(line.to_string());
            Ok(())
        }

        fn recv(&mut self) -> Option<Result<String, String>> {
            self.script.pop_front().map(Ok)
        }

        fn finish(&mut self) {
            self.finished = true;
            self.leftover_at_finish = self.script.len();
            self.commit();
        }
    }

    impl Drop for MockIo {
        fn drop(&mut self) {
            self.commit();
            if self.leftover_at_finish > 0 {
                panic!(
                    "MockIo：finish 时脚本仍剩余 {} 行未消费（脚本期望与实现行为不符）：{:?}",
                    self.leftover_at_finish, self.script
                );
            }
        }
    }

    /// mock spawn 缝：脚本队列（每次 spawn 弹一枚实例脚本；耗尽 → spawn 失败）+ 共享日志
    fn mock_spawn(scripts: Vec<Vec<String>>) -> (SpawnSeam, Arc<Mutex<SpawnLog>>) {
        let queue = Arc::new(Mutex::new(VecDeque::from(scripts)));
        let log = Arc::new(Mutex::new(SpawnLog::default()));
        let seam_log = Arc::clone(&log);
        let seam: SpawnSeam = Arc::new(move |spec: &SpawnSpec| {
            seam_log.lock().unwrap().specs.push(spec.clone());
            let script = queue
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "mock：脚本耗尽（未预期的额外 spawn）".to_string())?;
            Ok(Box::new(MockIo {
                script: VecDeque::from(script),
                sent: Vec::new(),
                finished: false,
                committed: false,
                log: Arc::clone(&seam_log),
                leftover_at_finish: 0,
            }) as Box<dyn AppServerIo>)
        });
        (seam, log)
    }

    fn sent_frames(log: &Arc<Mutex<SpawnLog>>) -> Vec<String> {
        log.lock().unwrap().instances[0].sent.clone()
    }

    fn sent_json(log: &Arc<Mutex<SpawnLog>>, idx: usize) -> serde_json::Value {
        serde_json::from_str(&sent_frames(log)[idx]).unwrap()
    }

    // ============================================================
    // 用例（任务书清单逐名落实）
    // ============================================================

    /// 裸方言主断言：响应帧无 `jsonrpc` 字段也按 id/result 识别；错误帧同方言；
    /// `jsonrpc` 在场不拒（不依赖它）；非 JSON 行归未知不猜
    #[test]
    fn bare_dialect_frame_parses_without_jsonrpc_field() {
        assert_eq!(
            parse_frame(r#"{"id":1,"result":{"userAgent":"codex-tui/0.162.1"}}"#),
            WireFrame::Response {
                id: 1,
                payload: FramePayload::Ok(serde_json::json!({"userAgent": "codex-tui/0.162.1"}))
            }
        );
        assert!(matches!(
            parse_frame(r#"{"id":2,"error":{"code":-32601,"message":"method not found"}}"#),
            WireFrame::Response {
                id: 2,
                payload: FramePayload::Err(FrameError { code: -32601, .. })
            }
        ));
        // 不按 2.0 严格校验：jsonrpc 字段在场也照解析（分类只看 id/result/method）
        assert!(matches!(
            parse_frame(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#),
            WireFrame::Response { id: 3, .. }
        ));
        assert_eq!(parse_frame("noise not json"), WireFrame::Unknown);
        assert_eq!(parse_frame(""), WireFrame::Unknown);
    }

    /// 通知旁流（无 id）与响应帧分道；ServerRequest（P2 扩展缝）第三道收集——
    /// 三道互不污染，响应按 id 精确匹配
    #[test]
    fn notification_stream_separated_from_responses() {
        let (seam, _log) = mock_spawn(vec![vec![
            init_ok(),
            note("configWarning"),
            note("remoteControl/status/changed"),
            server_req(900, "item/commandExecution/requestApproval"),
            resp(
                2,
                r#"{"thread":{"id":"t","status":{"type":"notLoaded"},"canAcceptDirectInput":false,"originator":"codex_work_desktop","source":"vscode"}}"#,
            ),
        ]]);
        let mut s = AppServerSession::start(&seam, &spec(), &client_info()).unwrap();
        let meta = s.read_thread_meta(TID).unwrap();
        s.close();
        assert_eq!(meta.status_type.as_deref(), Some("notLoaded"));
        assert_eq!(meta.can_accept_direct_input, Some(false));
        assert_eq!(meta.originator.as_deref(), Some("codex_work_desktop"));
        assert_eq!(
            s.notifications,
            vec![
                "configWarning".to_string(),
                "remoteControl/status/changed".to_string()
            ],
            "无 id 旁流分道收集，绝不参与响应匹配"
        );
        assert_eq!(
            s.server_requests,
            vec![(900, "item/commandExecution/requestApproval".to_string())],
            "ServerRequest 第三道（P2 审批应答扩展缝：只收集不代答）"
        );
        assert_eq!(s.unknown_frames, 0);
        assert_eq!(s.stray_responses, 0);
    }

    /// initialize 失败一律通道错误面（spawn 失败 / EOF 零输出 / 错误帧应答）
    #[test]
    fn initialize_failure_surfaces_channel_error() {
        // 形态 a：EOF——spawn 成功但零输出即关
        let (seam, _) = mock_spawn(vec![vec![]]);
        let err = AppServerSession::start(&seam, &spec(), &client_info())
            .err()
            .expect("EOF 无响应必须失败");
        assert!(
            matches!(err, AppServerError::Channel(ref m) if m.contains("EOF")),
            "EOF 无响应 → Channel：{err:?}"
        );
        // 形态 b：错误帧应答 initialize
        let (seam, _) = mock_spawn(vec![vec![resp_err(1, -32000, "Server error")]]);
        let err = AppServerSession::start(&seam, &spec(), &client_info())
            .err()
            .expect("错误帧应答 initialize 必须失败");
        assert!(
            matches!(err, AppServerError::Channel(ref m) if m.contains("initialize")),
            "握手失败一律通道面（不是方法语义）：{err:?}"
        );
        // 形态 c：spawn 失败本身（脚本耗尽）
        let (seam, _) = mock_spawn(vec![]);
        let err = AppServerSession::start(&seam, &spec(), &client_info())
            .err()
            .expect("spawn 失败必须失败");
        assert!(matches!(err, AppServerError::Channel(_)), "{err:?}");
    }

    /// -32601 形态 → unavailable（不 panic、不误判成功、不冒充通道错误）
    #[test]
    fn method_absent_treated_as_unavailable() {
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            resp_err(2, -32601, "Method not found: thread/read"),
        ]]);
        let err = client_of(seam).read_thread(None, TID).unwrap_err();
        match &err {
            AppServerError::MethodUnavailable { method, message } => {
                assert_eq!(method, "thread/read");
                assert!(message.contains("Method not found"));
            }
            other => panic!("-32601 必须归 unavailable：{other:?}"),
        }
        assert_eq!(err.wire_word(), "method_unavailable");
    }

    /// 档位写回执核对三态：命中（读回==请求档）/ 不符（读回为别档）/ 未生效（读不到）
    #[test]
    fn readback_three_states_hit_mismatch_ineffective() {
        fn switch_script() -> Vec<String> {
            vec![
                init_ok(),
                resp(2, r#"{"status":{"type":"idle"}}"#),
                resp(
                    3,
                    r#"{"turnId":"turn-1","turn":{"id":"turn-1","status":"inProgress"}}"#,
                ),
                turn_completed("completed"),
            ]
        }
        // 命中
        let (seam, _) = mock_spawn(vec![switch_script()]);
        let receipt = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "。",
                &readback_of(Some("never")),
            )
            .unwrap();
        assert_eq!(receipt.verify, PolicyVerify::Hit);
        assert_eq!(receipt.requested, "never");
        assert_eq!(receipt.turn_id.as_deref(), Some("turn-1"));
        assert!(receipt.turn_completion.observed);
        // 不符（读回为别档——前端不采信本地记忆、重拉 GET 的后端依据）
        let (seam, _) = mock_spawn(vec![switch_script()]);
        let receipt = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "。",
                &readback_of(Some("on-request")),
            )
            .unwrap();
        assert_eq!(
            receipt.verify,
            PolicyVerify::Mismatch {
                readback: "on-request".to_string()
            }
        );
        // 未生效（读不到——rollout 无 turn_context / 不可读；不谎报生效）
        let (seam, _) = mock_spawn(vec![switch_script()]);
        let receipt = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "。",
                &readback_of(None),
            )
            .unwrap();
        assert_eq!(receipt.verify, PolicyVerify::Ineffective);
        // M1 宽容判定（先红）：读回串带空白 / 大小写漂移 → Hit，不误报 Mismatch
        for drifted in [" never ", "NEVER"] {
            let (seam, _) = mock_spawn(vec![switch_script()]);
            let receipt = client_of(seam)
                .switch_approval_policy(
                    TID,
                    AppApprovalPolicy::Never,
                    PolicyCarrier::TurnStart,
                    "。",
                    &readback_of(Some(drifted)),
                )
                .unwrap();
            assert_eq!(
                receipt.verify,
                PolicyVerify::Hit,
                "读回串漂移（{drifted:?}）不应误报 Mismatch"
            );
        }
    }

    /// 质量 M4（先红）：**未观测到回合收口**（`turn/completed` 直到 EOF 都没来 ⇒
    /// observed=false）时，读回缝即便给「别档」值也**不得**产出 Mismatch——本回合
    /// `turn_context` 未必落盘，缝里读到的可能仍是**旧回合**上下文，比对就是假不符；
    /// 按既有三态降级 `Ineffective`（口径同 ②' resume-only：不查读回缝）。
    /// 对照腿证明降级闸只认「观测态」：同一「别档」读回值在 observed=true 时照旧 Mismatch
    #[test]
    fn unobserved_turn_completion_downgrades_verify_to_ineffective() {
        /// 到 turn/start 应答为止——turn/completed 永不投递（等待窗 EOF 收口）
        fn no_completion_script() -> Vec<String> {
            vec![
                init_ok(),
                resp(2, r#"{"status":{"type":"idle"}}"#),
                resp(
                    3,
                    r#"{"turnId":"turn-1","turn":{"id":"turn-1","status":"inProgress"}}"#,
                ),
            ]
        }
        fn completed_script() -> Vec<String> {
            let mut s = no_completion_script();
            s.push(turn_completed("completed"));
            s
        }
        // 未观测收口 + 读回为别档：必须 Ineffective（不是 Mismatch——假不符会误导前端）
        let (seam, _) = mock_spawn(vec![no_completion_script()]);
        let receipt = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "。",
                &readback_of(Some("on-request")),
            )
            .unwrap();
        assert!(
            !receipt.turn_completion.observed,
            "turn/completed 直到 EOF 未投递——如实未观测"
        );
        assert_eq!(
            receipt.verify,
            PolicyVerify::Ineffective,
            "未观测到回合收口 ⇒ 读回降级 Ineffective（别档读回值不得产出假 Mismatch）"
        );
        // 对照腿：同一读回值、观测到收口 → 照旧 Mismatch（降级只由观测态触发）
        let (seam, _) = mock_spawn(vec![completed_script()]);
        let receipt = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "。",
                &readback_of(Some("on-request")),
            )
            .unwrap();
        assert!(receipt.turn_completion.observed);
        assert_eq!(
            receipt.verify,
            PolicyVerify::Mismatch {
                readback: "on-request".to_string()
            },
            "观测到收口时读回为别档照旧如实报不符"
        );
    }

    /// interrupt 回执 + turnId 校验：参数逐字进帧；回执回显 thread/turn + 响应态；
    /// 自持等待窗观测 turn/completed(interrupted)；空 turnId 投递前拒（零协议副作用）
    #[test]
    fn interrupt_receipt_shapes() {
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            resp(2, r#"{"turn":{"id":"turn-9","status":"inProgress"}}"#),
            turn_completed("interrupted"),
        ]]);
        let mut s = AppServerSession::start(&seam, &spec(), &client_info()).unwrap();
        let r = s.interrupt(TID, "turn-9").unwrap();
        let done = s.wait_turn_completed();
        s.close();
        assert_eq!(r.thread_id, TID);
        assert_eq!(r.turn_id, "turn-9");
        assert_eq!(r.response_status.as_deref(), Some("inProgress"));
        assert!(done.observed, "自持连接等待窗内观测到完成通知");
        assert_eq!(done.status.as_deref(), Some("interrupted"));
        assert_eq!(done.duration_ms, Some(6032));
        // 发出的帧：turnId 逐字进 params（第 2 帧，第 1 帧是 initialize）
        let frame = sent_json(&log, 1);
        assert_eq!(frame["method"], "turn/interrupt");
        assert_eq!(frame["params"]["threadId"], TID);
        assert_eq!(frame["params"]["turnId"], "turn-9");
        // 空 turnId → 投递前拒（InvalidTarget），只发过 initialize
        let (seam2, log2) = mock_spawn(vec![vec![init_ok()]]);
        let mut s2 = AppServerSession::start(&seam2, &spec(), &client_info()).unwrap();
        assert!(matches!(
            s2.interrupt(TID, "  "),
            Err(AppServerError::InvalidTarget(_))
        ));
        s2.close();
        assert_eq!(
            log2.lock().unwrap().instances[0].sent.len(),
            1,
            "空 turnId 在 spawn 后、发送前拒——只发过 initialize"
        );
    }

    /// Task 6b 中止臂接线（单测面）：等待窗探针触发 → **同连接**发 `turn/interrupt`
    /// （帧带 threadId/turnId——跨连接会被写者锁拒，interrupt 只许持连侧发）→ 完成
    /// 帧 status=interrupted 自然入 TurnCompletion；turnId 缺席 → 无可中止对象按未
    /// 观测收口（零 interrupt 帧——不猜目标）
    #[test]
    fn cancel_probe_fires_self_held_interrupt_during_wait() {
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            // turn/interrupt(id=2) 的响应帧（call 按 id 匹配——探针先于首帧触发）
            resp(2, r#"{"turn":{"status":"interrupting"}}"#),
            turn_completed("interrupted"),
        ]]);
        let mut s = AppServerSession::start(&seam, &spec(), &client_info()).unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let probe: CancelProbe = {
            let f = flag.clone();
            std::sync::Arc::new(move || f.load(std::sync::atomic::Ordering::SeqCst))
        };
        let done = s.wait_turn_completed_cancellable(Some(&probe), TID, Some("turn-6b"));
        s.close();
        assert!(done.observed, "interrupt 后继续等收口（完成帧入账）");
        assert_eq!(done.status.as_deref(), Some("interrupted"));
        // 第 2 帧（第 1 帧是 initialize）= 持连侧 interrupt：threadId/turnId 逐字进帧
        let frame = sent_json(&log, 1);
        assert_eq!(frame["method"], "turn/interrupt");
        assert_eq!(frame["params"]["threadId"], TID);
        assert_eq!(frame["params"]["turnId"], "turn-6b");
        // turnId 缺席腿：探针再触发也无目标 → 未观测收口、零 interrupt 帧
        let (seam2, log2) = mock_spawn(vec![vec![init_ok()]]);
        let mut s2 = AppServerSession::start(&seam2, &spec(), &client_info()).unwrap();
        let done2 = s2.wait_turn_completed_cancellable(Some(&probe), TID, None);
        s2.close();
        assert!(!done2.observed, "无可中止对象——按未观测收口（不猜目标）");
        assert_eq!(
            log2.lock().unwrap().instances[0].sent.len(),
            1,
            "turnId 缺席零 interrupt 帧（只发过 initialize）"
        );
    }

    /// Task 6b review M-4：interrupt 响应是 error 帧（版本漂移 -32601 形态）→ 发送
    /// 失败**不中止等待**——回合自然收口、observed 如实（不假装已中止、也不丢完成
    /// 观测）；interrupt 帧确实发过（失败是通道侧回答，不是没发）
    #[test]
    fn interrupt_error_frame_keeps_waiting_and_observes_honestly() {
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            // turn/interrupt(id=2) 的响应 = error 帧（-32601 method not found 形态）
            resp_err(2, -32601, "Method not found: turn/interrupt"),
            turn_completed("completed"),
        ]]);
        let mut s = AppServerSession::start(&seam, &spec(), &client_info()).unwrap();
        let probe: CancelProbe = std::sync::Arc::new(|| true);
        let done = s.wait_turn_completed_cancellable(Some(&probe), TID, Some("turn-err"));
        s.close();
        assert!(done.observed, "interrupt 失败不中止等待——完成帧照常观测");
        assert_eq!(done.status.as_deref(), Some("completed"));
        // 证据面：interrupt 帧确实发过（threadId/turnId 逐字）
        let frame = sent_json(&log, 1);
        assert_eq!(frame["method"], "turn/interrupt");
        assert_eq!(frame["params"]["threadId"], TID);
        assert_eq!(frame["params"]["turnId"], "turn-err");
    }

    /// 质量 M5 / M22 吞帧竞窗（先红）：`turn/completed` **先于** interrupt 应答到达
    /// ——`interrupt()` 的 call() 只认 id 匹配帧，完成帧先落旁流；旧行为外层此后只会
    /// EOF（脚本已尽）⇒ observed=false，「回合已完成」这一事实被吞掉。修复后由留档位
    /// 补检认领：observed=true **且** status/durationMs 一并复原（旁流只留方法名，
    /// 复原不出完成态——这正是必须留档而非事后扫旁流的理由）
    #[test]
    fn turn_completed_before_interrupt_response_is_observed() {
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            // 竞窗：完成帧先到（id=2 的 interrupt 应答还没来）
            turn_completed("interrupted"),
            resp(2, r#"{"turn":{"status":"interrupting"}}"#),
        ]]);
        let mut s = AppServerSession::start(&seam, &spec(), &client_info()).unwrap();
        let probe: CancelProbe = std::sync::Arc::new(|| true);
        let done = s.wait_turn_completed_cancellable(Some(&probe), TID, Some("turn-1"));
        s.close();
        assert!(
            done.observed,
            "先到的完成帧不得被 call() 吞掉（M22 竞窗——外层等不到第二帧）"
        );
        assert_eq!(done.status.as_deref(), Some("interrupted"));
        assert_eq!(done.duration_ms, Some(6032));
        // 既有旁流语义不变：该通知照旧进 notifications（不去重、不由本层取走）
        assert!(
            s.notifications.iter().any(|m| m == "turn/completed"),
            "通知旁流照收（消费语义不变）：{:?}",
            s.notifications
        );
        // 探针确实发过 interrupt（完成帧在应答之前，故 interrupt 已在飞——竞窗前提成立）
        let frame = sent_json(&log, 1);
        assert_eq!(frame["method"], "turn/interrupt");
        assert_eq!(frame["params"]["turnId"], "turn-1");
    }

    /// 活跃线程写拒 → 降级旗标/错误如实（-32600 + active writer 实测形态）；
    /// 模块无 daemon 全局态（【已删】）——写拒不毒化后续读动作
    #[test]
    fn active_thread_write_unavailable_flag_surfaces_degraded() {
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            resp_err(2, -32600, "thread 01a12b82 already has an active writer"),
        ]]);
        let err = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "。",
                &readback_of(None),
            )
            .unwrap_err();
        assert_eq!(err, AppServerError::ActiveThreadWriteUnavailable);
        assert_eq!(
            err.wire_word(),
            ACTIVE_THREAD_WRITE_UNAVAILABLE,
            "降级旗标词（审计/回执分类列）"
        );
        assert!(err.to_string().contains("APP"), "用户面降级文案在场：{err}");
        // 写拒不毒化读：同一客户端后续动作独立（每动作一枚新实例，无全局降级态）
        let (seam, log) = mock_spawn(vec![
            vec![
                init_ok(),
                resp_err(2, -32600, "thread 01a12b82 already has an active writer"),
            ],
            vec![
                init_ok(),
                resp(2, r#"{"thread":{"id":"x","status":{"type":"notLoaded"}}}"#),
            ],
        ]);
        let client = client_of(seam);
        assert!(matches!(
            client.switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "。",
                &readback_of(None)
            ),
            Err(AppServerError::ActiveThreadWriteUnavailable)
        ));
        assert!(
            client.read_thread(None, TID).is_ok(),
            "写拒后读照常——跨实例只读不受写者锁影响（探测 §1.4）"
        );
        assert_eq!(log.lock().unwrap().specs.len(), 2, "每动作一枚新实例");
    }

    /// 短命生命周期：每动作一个实例，spawn→initialize→…→finish 逐实例成立（裁决 32a）
    #[test]
    fn per_action_session_spawns_and_exits() {
        fn read_script() -> Vec<String> {
            vec![init_ok(), resp(2, r#"{"thread":{"id":"t"}}"#)]
        }
        let (seam, log) = mock_spawn(vec![read_script(), read_script()]);
        let client = client_of(seam);
        client.read_thread(None, TID).unwrap();
        client.read_thread(None, TID).unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.specs.len(), 2, "两个动作 = 两枚实例（无常驻池）");
        assert_eq!(log.instances.len(), 2);
        for inst in &log.instances {
            assert!(
                inst.finished,
                "每枚实例都被显式收尾（优雅退出——写者锁纪律）"
            );
            let first: serde_json::Value = serde_json::from_str(&inst.sent[0]).unwrap();
            assert_eq!(
                first["method"], "initialize",
                "每实例第一帧 = initialize 握手"
            );
        }
    }

    /// UUID 映射复用 rollout 映射（锚点 codex.rs:263-289）：文件名权威 → UUID 会话号回退
    /// → 都无则拒；经 read_thread 接线时 threadId 参数取自文件名（对漂移不静默）
    #[test]
    fn thread_uuid_resolution_reuses_rollout_mapping() {
        // ① rollout 文件名 UUID = 权威源（会话号漂移也按文件名投递）
        assert_eq!(
            resolve_thread(Some(ROLLOUT), "drifted-session-id")
                .unwrap()
                .id,
            OLD_UUID
        );
        // ② 无 rollout 路径时仅有 UUID 形态的会话号可回退
        assert_eq!(resolve_thread(None, OLD_UUID).unwrap().id, OLD_UUID);
        // ③ 都无 UUID → None（调用方如实拒，绝不按会话名猜）
        assert!(resolve_thread(None, "我的会话").is_none());
        // ④ 经 read_thread 的接线断言：thread/read 的 threadId = 文件名 UUID
        let (seam, log) = mock_spawn(vec![vec![init_ok(), resp(2, r#"{"thread":{"id":"x"}}"#)]]);
        client_of(seam)
            .read_thread(Some(ROLLOUT), "drifted-session-id")
            .unwrap();
        let frame = sent_json(&log, 1);
        assert_eq!(frame["method"], "thread/read");
        assert_eq!(
            frame["params"]["threadId"], OLD_UUID,
            "threadId 必须来自 rollout 文件名（权威源），不是会话号"
        );
        // ⑤ 解析失败 → InvalidTarget 且零 spawn（投递前拒，零进程零协议副作用）
        let (seam, log) = mock_spawn(vec![]);
        assert!(matches!(
            client_of(seam).read_thread(None, "no-uuid"),
            Err(AppServerError::InvalidTarget(_))
        ));
        assert!(
            log.lock().unwrap().specs.is_empty(),
            "未解析出线程就不 spawn"
        );
    }

    /// 载体钉值（T0.2）：主载体① turn/start.approvalPolicy（input 必填附极小输入）；
    /// 兜底② thread/resume.approvalPolicy 有缝有断言；载体间不重复携带；空输入 spawn 前拒
    #[test]
    fn approval_policy_write_uses_probed_carrier() {
        fn switch_script() -> Vec<String> {
            vec![
                init_ok(),
                resp(2, r#"{"status":{"type":"idle"}}"#),
                resp(
                    3,
                    r#"{"turnId":"turn-1","turn":{"id":"turn-1","status":"inProgress"}}"#,
                ),
                turn_completed("completed"),
            ]
        }
        // 主载体①：turn/start.approvalPolicy
        let (seam, log) = mock_spawn(vec![switch_script()]);
        let receipt = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Untrusted,
                PolicyCarrier::TurnStart,
                "切换档位探测",
                &readback_of(Some("untrusted")),
            )
            .unwrap();
        assert_eq!(receipt.carrier, PolicyCarrier::TurnStart);
        assert_eq!(receipt.verify, PolicyVerify::Hit);
        let resume = sent_json(&log, 1);
        assert_eq!(resume["method"], "thread/resume");
        assert!(
            resume["params"].get("approvalPolicy").is_none(),
            "载体①时 resume 不带档位（兜底②才带）"
        );
        let turn = sent_json(&log, 2);
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(
            turn["params"]["approvalPolicy"], "untrusted",
            "主载体①：档位挂在 turn/start"
        );
        assert_eq!(turn["params"]["threadId"], TID);
        assert_eq!(turn["params"]["input"][0]["type"], "text");
        assert_eq!(
            turn["params"]["input"][0]["text"], "切换档位探测",
            "input 必填——纯切档附极小输入（T0.2 主载体代价）"
        );
        // 兜底载体②：thread/resume.approvalPolicy（线程级一次 resume 长效；持久层跟随）
        let (seam, log) = mock_spawn(vec![switch_script()]);
        let receipt = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Untrusted,
                PolicyCarrier::Resume,
                "切换档位探测",
                &readback_of(Some("untrusted")),
            )
            .unwrap();
        assert_eq!(receipt.carrier, PolicyCarrier::Resume);
        assert_eq!(receipt.verify, PolicyVerify::Hit);
        let resume = sent_json(&log, 1);
        assert_eq!(
            resume["params"]["approvalPolicy"], "untrusted",
            "兜底②：档位挂在 resume"
        );
        let turn = sent_json(&log, 2);
        assert!(
            turn["params"].get("approvalPolicy").is_none(),
            "兜底②时 turn/start 不重复带档位"
        );
        // 空输入：spawn 前拒（turn/start input 必填的诚实面——不起回合不烧 token）
        let (seam, log) = mock_spawn(vec![]);
        assert!(matches!(
            client_of(seam).switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "  ",
                &readback_of(None)
            ),
            Err(AppServerError::InvalidTarget(_))
        ));
        assert!(log.lock().unwrap().specs.is_empty(), "空输入零 spawn");
    }

    /// ②' resume-only 兜底（review I1）：-32601 消费场景——只 resume 携档（线程级
    /// 持久、后续回合生效）、**不起 turn/start**（零回合成本）；核对恒 Ineffective
    /// （读回缝即便给「命中值」也不查——rollout 尾部仍是旧回合上下文，比对会误报
    /// Mismatch）；turn 无从观测（observed=false 构造使然）；空 input 放行（无回合
    /// 载体不要求 input——前置只约束会起回合的载体）。
    #[test]
    fn resume_only_carrier_skips_turn_start_and_reports_ineffective() {
        // 读回缝故意给「命中值」——resume-only 仍必须 Ineffective（不查缝的自证）
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            resp(2, r#"{"status":{"type":"idle"}}"#),
        ]]);
        let receipt = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::ResumeOnly,
                " ", // 空 input 放行：resume-only 无回合 ⇒ 无必填
                &readback_of(Some("never")),
            )
            .unwrap();
        assert_eq!(receipt.carrier, PolicyCarrier::ResumeOnly);
        assert_eq!(
            receipt.verify,
            PolicyVerify::Ineffective,
            "恒 Ineffective——不查读回缝（缝给了命中值也不采信）"
        );
        assert_eq!(receipt.requested, "never");
        assert_eq!(receipt.turn_id, None);
        assert!(
            !receipt.turn_completion.observed,
            "无回合无从观测（构造使然）"
        );
        let frames = sent_frames(&log);
        assert_eq!(
            frames.len(),
            2,
            "一枚实例只发 initialize + thread/resume 后即 close"
        );
        let resume = sent_json(&log, 1);
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(
            resume["params"]["approvalPolicy"], "never",
            "档位挂 resume（线程级持久）"
        );
        assert!(
            !frames.iter().any(|f| f.contains("turn/start")),
            "不起 turn/start（-32601 兜底不白付回合）"
        );
    }

    /// 附加钉值：裸 spawn argv 的 Windows 垫片包装（生产 spawn 形态的纯核面——
    /// `.cmd` 垫片经 `cmd /c` 转一手，`.exe` 直 spawn）
    #[test]
    fn bare_argv_wraps_windows_shim_and_direct_spawns_exe() {
        assert_eq!(
            bare_argv("C:/tools/codex.cmd", "windows"),
            vec!["cmd", "/c", "C:/tools/codex.cmd", "app-server"]
        );
        assert_eq!(
            bare_argv("C:/tools/codex.exe", "windows"),
            vec!["C:/tools/codex.exe", "app-server"]
        );
        assert_eq!(
            bare_argv("/usr/local/bin/codex", "macos"),
            vec!["/usr/local/bin/codex", "app-server"]
        );
    }

    /// 附加钉值：档位词表单点（wire ↔ parse 同词表；ALL = 三枚举——Task 6a supported 数据源）
    #[test]
    fn approval_policy_wire_words_are_pinned() {
        // M1 宽容判定钉值（先红）：读回串带空白/大小写漂移 → 仍解析成功
        assert_eq!(
            AppApprovalPolicy::parse(" NEVER "),
            Some(AppApprovalPolicy::Never)
        );
        assert_eq!(
            AppApprovalPolicy::parse("On-Request"),
            Some(AppApprovalPolicy::OnRequest)
        );
        assert_eq!(AppApprovalPolicy::Never.wire(), "never");
        assert_eq!(AppApprovalPolicy::OnRequest.wire(), "on-request");
        assert_eq!(AppApprovalPolicy::Untrusted.wire(), "untrusted");
        for p in AppApprovalPolicy::ALL {
            assert_eq!(AppApprovalPolicy::parse(p.wire()), Some(p));
        }
        assert_eq!(
            AppApprovalPolicy::parse("granular"),
            None,
            "结构化变体不在本批——不猜"
        );
    }

    /// 降级/不可用文案钉句（spec Minor·对齐计划回填钉句——APP 词汇表纪律，去协议术语括注）
    #[test]
    fn degraded_copy_pins_plan_wording() {
        assert_eq!(
            ACTIVE_THREAD_WRITE_REASON,
            "会话正被 APP 使用，请关闭 APP 后重试或在 APP 内操作"
        );
        assert_eq!(
            INTERRUPT_NOT_SELF_HELD_REASON,
            "活跃回合由 APP 持有写者，暂无协议中止通道"
        );
        // 空输入拒文案覆盖两载体（先红——现状是主载体①专属措辞）
        let (seam, _) = mock_spawn(vec![]);
        let err = client_of(seam)
            .switch_approval_policy(
                TID,
                AppApprovalPolicy::Never,
                PolicyCarrier::TurnStart,
                "  ",
                &readback_of(None),
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "目标不可用：CODEX_APP_POLICY_INPUT_REQUIRED: 档位切换需附极小输入（协议回合 input 必填，会起回合的载体同规）"
        );
    }

    /// stderr 尾行环形缓冲（spec Major 1 的纯核面）：只留末 cap 行、保序、拼接诊断面
    #[test]
    fn stderr_tail_keeps_last_lines_in_order() {
        let mut tail = StderrTail::with_cap(8);
        for i in 0..10 {
            tail.push(format!("line-{i}"));
        }
        assert_eq!(
            tail.joined(),
            "line-2
line-3
line-4
line-5
line-6
line-7
line-8
line-9",
            "超上限丢最旧、保序拼接"
        );
        let mut small = StderrTail::with_cap(8);
        for i in 0..3 {
            small.push(format!("s-{i}"));
        }
        assert_eq!(
            small.joined(),
            "s-0
s-1
s-2",
            "不足上限全保留"
        );
        assert_eq!(StderrTail::with_cap(8).joined(), "", "空缓冲 = 空串");
    }

    /// M5 守卫（先红）：finish 时脚本仍有剩余行 → Drop 时 panic（期望与实现不符当场炸）
    #[test]
    #[should_panic(expected = "脚本仍剩余")]
    fn mock_io_finish_with_leftover_panics_on_drop() {
        let (seam, log) = mock_spawn(vec![vec![init_ok(), note("never-consumed")]]);
        let mut s = AppServerSession::start(&seam, &spec(), &client_info()).unwrap();
        // 只消费了 initialize 响应就 close——脚本第 2 行永远没人读
        s.close();
        drop(s);
        assert_eq!(log.lock().unwrap().instances.len(), 1);
    }

    // ============================================================
    // Task 7：app-server 可用性探测（spawn + initialize + 方法在场性，零模型成本）
    // ============================================================

    /// 探测 exe 夹具（tempdir stub——ProbeKey 走真路径 stat，需要在场文件才能进执行体）
    fn probe_exe_fixture() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("codex.exe");
        std::fs::write(&exe, "stub").unwrap();
        (dir, exe.to_string_lossy().to_string())
    }

    /// 全在场探测脚本：initialize + 4 方法各一应答帧（形态故意混搭：Ok / -32602 参数
    /// 面错 / -32600 writer 锁——三种非 -32601 应答都是「在场」证据；ping 顺序见
    /// [`PROBE_PRESENCE_METHODS`]：read → resume → start → interrupt）
    fn probe_script() -> Vec<String> {
        vec![
            init_ok(),
            // thread/read（id=2）：Ok = 在场
            resp(2, r#"{"thread":{"id":"t"}}"#),
            // thread/resume（id=3）：Ok = 在场
            resp(3, r#"{"status":{"type":"idle"}}"#),
            // turn/start（id=4）：-32602 参数面错 = 在场（空参 ping 无 threadId/input，
            // 必被参数面拒——探针不起真实回合，零模型成本）
            resp_err(4, -32602, "missing required field: threadId"),
            // turn/interrupt（id=5）：-32600 + 单写者锁特征串 = 在场（方法分派后的
            // 语义拒——Task 7 专项：与 -32601 必须分清）
            resp_err(5, -32600, "thread 01a12b82 already has an active writer"),
        ]
    }

    /// 计划 Task 7 主断言：initialize + 4 方法在场 → Pass；**零模型成本钉面**：方法
    /// ping 帧 params 恒空对象（turn/start 不带 threadId/input——不起真实回合）；
    /// 混搭应答形态（Ok / -32602 / -32600 writer 锁）都算在场
    #[test]
    fn codex_appserver_probe_passes_on_initialize_and_methods_present() {
        let (_dir, exe) = probe_exe_fixture();
        let (seam, log) = mock_spawn(vec![probe_script()]);
        let cache = ProbeCache::default();
        let v = probe_app_server(&exe, &seam, &spec(), &client_info(), &cache);
        assert!(v.is_pass(), "{v:?}");
        assert!(
            matches!(&v, ProbeVerdict::Pass { detail, .. }
                if detail.contains("4") && detail.contains("在场")),
            "结论带方法数与「在场」措辞：{v:?}"
        );
        let log = log.lock().unwrap();
        assert_eq!(log.specs.len(), 1, "一枚短命实例");
        assert!(log.instances[0].finished, "探测完优雅收尾（写者锁纪律）");
        let start_frame = log.instances[0]
            .sent
            .iter()
            .find(|f| f.contains("turn/start"))
            .expect("turn/start ping 帧在场");
        let frame: serde_json::Value = serde_json::from_str(start_frame).unwrap();
        assert_eq!(
            frame["params"],
            serde_json::json!({}),
            "方法 ping 空参——绝不起真实回合（零模型成本）"
        );
    }

    /// -32601 → 探测 fail（unavailable + 原因）：缺席方法名进 hint；缺席后短路
    /// （后续方法不再 ping）；失败路径也优雅收尾；结论进缓存（5min TTL 族）
    #[test]
    fn codex_appserver_probe_fails_when_method_absent() {
        let (_dir, exe) = probe_exe_fixture();
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            resp(2, r#"{"thread":{"id":"t"}}"#),
            resp_err(3, -32601, "Method not found: thread/resume"),
        ]]);
        let cache = ProbeCache::default();
        let v = probe_app_server(&exe, &seam, &spec(), &client_info(), &cache);
        assert!(!v.is_pass(), "{v:?}");
        let hint = v.hint().unwrap_or_default();
        assert!(hint.contains("thread/resume"), "缺席方法名进原因：{hint}");
        assert!(
            hint.contains("-32601") || hint.contains("Method not found"),
            "缺席判据（schema 面）进原因：{hint}"
        );
        let log = log.lock().unwrap();
        assert_eq!(
            log.instances[0].sent.len(),
            3,
            "缺席即短路：只发过 initialize + 两个 ping"
        );
        assert!(log.instances[0].finished, "失败路径也优雅收尾");
        assert_eq!(cache.len(), 1, "失败结论也进缓存（FAILURE_TTL 族）");
    }

    /// 成功长缓存：命中后零 respawn（脚本只备一枚——第二次 spawn 会因脚本耗尽而
    /// 失败，仍返回 pass 即「缓存命中未重探」的机器证明）
    #[test]
    fn codex_appserver_probe_cached_success_skips_respawn() {
        let (_dir, exe) = probe_exe_fixture();
        let (seam, log) = mock_spawn(vec![probe_script()]);
        let cache = ProbeCache::default();
        let v1 = probe_app_server(&exe, &seam, &spec(), &client_info(), &cache);
        assert!(v1.is_pass());
        let v2 = probe_app_server(&exe, &seam, &spec(), &client_info(), &cache);
        assert!(
            v2.is_pass(),
            "缓存命中不重探（脚本已耗尽——respawn 必失败，pass 即零 respawn）：{v2:?}"
        );
        assert_eq!(log.lock().unwrap().specs.len(), 1, "只 spawn 过一枚实例");
        assert_eq!(cache.len(), 1);
    }

    /// 失败 TTL（5min 口径与 zcode/codex CLI 探针同款）：TTL 内失败结论原样命中
    /// （零 spawn——通道坏着时不高频重探）；过期即重探（抖动恢复结论可上线）
    #[test]
    fn codex_appserver_probe_failure_ttl_5min() {
        use super::super::gate::FAILURE_TTL_MS;
        let now_ms = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };
        let (_dir, exe) = probe_exe_fixture();
        let key = ProbeKey::of_exe(&exe);
        let cache = ProbeCache::default();
        // ① TTL 内：失败结论原样命中，执行体零调用（零 spawn）
        cache.put_at(key.clone(), ProbeVerdict::fail("瞬时抖动"), now_ms());
        let (seam, log) = mock_spawn(vec![]);
        let v = probe_app_server(&exe, &seam, &spec(), &client_info(), &cache);
        assert_eq!(
            v.hint(),
            Some("瞬时抖动"),
            "TTL 内原样复用失败结论（不重探不重写）：{v:?}"
        );
        assert!(
            log.lock().unwrap().specs.is_empty(),
            "TTL 内零 spawn（坏着时不重复付探测代价）"
        );
        // ② 过期：重探（脚本一枚）→ 抖动恢复结论上线并覆盖缓存
        cache.put_at(
            key.clone(),
            ProbeVerdict::fail("旧失败"),
            now_ms() - FAILURE_TTL_MS - 1,
        );
        let (seam, log) = mock_spawn(vec![probe_script()]);
        let v = probe_app_server(&exe, &seam, &spec(), &client_info(), &cache);
        assert!(v.is_pass(), "失败过期必须重探且恢复结论可用：{v:?}");
        assert_eq!(log.lock().unwrap().specs.len(), 1);
        assert!(
            cache.get(&key).is_some_and(|c| c.is_pass()),
            "恢复结论覆盖旧失败（成功长缓存）"
        );
    }

    /// **评审 M3：错误帧分类的大小写口径与 exec 侧同源**——旧实现在此处
    /// `message.contains(WRITER_LOCK_MARKER)` 是**大小写敏感**的，而 `codex.rs` 的
    /// `writer_lock_evidence` 先 lowercase：同一句锁告警换个大小写，一处判锁、一处判普通
    /// RPC 错误（口径漂移）。现在两侧共用 [`hits_writer_lock_marker`]。
    ///
    /// 变异验证：把 `classify` 改回大小写敏感的 `contains` → 本用例红
    /// （大写变体落到 `AppServerError::Rpc`）。
    #[test]
    fn writer_lock_classification_is_case_insensitive_like_the_exec_side() {
        let upper = FrameError {
            code: INVALID_REQUEST_CODE,
            message: "THREAD 01A12B82 ALREADY HAS AN ACTIVE WRITER".to_string(),
        };
        assert_eq!(
            classify("thread/read", upper),
            AppServerError::ActiveThreadWriteUnavailable,
            "大小写变体必须与 exec 侧同判（口径单点 hits_writer_lock_marker）"
        );
        let mixed = FrameError {
            code: INVALID_REQUEST_CODE,
            message: "Already Has An Active Writer".to_string(),
        };
        assert_eq!(
            classify("thread/read", mixed),
            AppServerError::ActiveThreadWriteUnavailable
        );
        // 安全面一字不动：**裸 -32600 不得定性为锁**（近似串/无特征串同理）
        for message in [
            "invalid params",
            "already has an active reader",
            "writer busy",
        ] {
            let e = FrameError {
                code: INVALID_REQUEST_CODE,
                message: message.to_string(),
            };
            assert!(
                matches!(classify("thread/read", e), AppServerError::Rpc { .. }),
                "无锁特征串的 -32600 必须原样透传为 Rpc（{message}）"
            );
        }
    }

    /// 探针语义专项（Task 7）：**-32600 + 单写者锁特征串 ≠ 方法缺席**——writer 锁
    /// 是方法分派后的语义拒，恰是在场证据；全部方法都回 writer 锁形态 → 探测照过
    /// （不误报缺席把通道判死）
    #[test]
    fn probe_writer_lock_response_counts_as_method_present() {
        let (_dir, exe) = probe_exe_fixture();
        let lock_resp =
            |id: u64| resp_err(id, -32600, "thread 01a12b82 already has an active writer");
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            lock_resp(2),
            lock_resp(3),
            lock_resp(4),
            lock_resp(5),
        ]]);
        let v = probe_app_server(&exe, &seam, &spec(), &client_info(), &ProbeCache::default());
        assert!(
            v.is_pass(),
            "writer 锁应答 = 方法在场（-32600 与 -32601 必须分清）：{v:?}"
        );
    }

    /// initialize 失败（EOF / spawn 失败）→ 探测 fail 且原因走通道面（握手没成
    /// 谈不上方法语义）；零方法 ping
    #[test]
    fn probe_initialize_failure_surfaces_channel_reason() {
        let (_dir, exe) = probe_exe_fixture();
        let (seam, log) = mock_spawn(vec![vec![]]); // EOF：spawn 成功但零输出即关
        let v = probe_app_server(&exe, &seam, &spec(), &client_info(), &ProbeCache::default());
        assert!(!v.is_pass());
        let hint = v.hint().unwrap_or_default();
        assert!(hint.contains("通道"), "握手失败归通道面：{hint}");
        let log = log.lock().unwrap();
        assert_eq!(
            log.instances[0].sent.len(),
            1,
            "只发过 initialize——零方法 ping"
        );
    }
}

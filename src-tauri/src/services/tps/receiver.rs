//! Claude 本机 OTLP 接收器（Spec §5 接收边界；2026-10-24 计划 Task 7，裁决 R10 新文件）。
//!
//! ## 边界逐条（Spec §5「Claude接收器」）
//!
//! * **只 bind 127.0.0.1**；默认固定端口 [`install::DEFAULT_RECEIVER_PORT`]（47181），
//!   首次被占则在 [`install::FALLBACK_PORT_RANGE`]（47182–47190）顺延——选择必须来自
//!   **真实 bind 尝试**（[`probe_free_port_in`]：对每个候选真的 `TcpListener::bind`
//!   一次，成功即选中并落盘；`pick_receiver_port` 的纯占用表形态只留给测试）；
//! * **持久化端口之后冲突 = 报状态**：[`bind_receiver_port`] 失败 →
//!   [`record_port_conflict`] 把 claude 的 sourceState 写成 ok=false +
//!   `tps-receiver-port-conflict` 前缀——**绝不杀进程、绝不静默换端口**使旧配置失效；
//! * OTLP **仅 JSON**、路径 [`install::INGEST_PATH`]（/v1/traces）、上限
//!   [`install::MAX_BODY_BYTES`]（1MiB）——超限在**缓冲完整 body 之前**拒绝
//!   （Content-Length 头预检 + axum `DefaultBodyLimit` 流式截断双闸）；
//! * **独立随机 Bearer**（[`install::verify_bearer`] 常量时间比对；与 API 认证无关）；
//! * 报文经 [`crate::services::tps::sources::claude::decode_trace_with_diagnostics`]
//!   解码 → 观察连同**诊断**一起提交为 claude 的 ReadBatch（诊断进 sourceState——
//!   成功属性漂移若静默丢弃，样本会无声缩水，Task 5 评审追记）；
//! * **随 Tuvis 退出而停**（监听任务绑定进程生命周期；`shutdown` 供总授权关闭与
//!   exit_cleanup 调用），不落任何系统守护进程。
//!
//! ## 测试纪律（brief 安全约束逐字）
//!
//! 测试**绝不 bind 47181–47190**：端口探测用例传临时候选（bind :0 由 OS 分配），
//! 接收守卫用例走 [`ingest`] 纯函数（连接注入），HTTP 面用 tower oneshot（不监听）。

use std::sync::LazyLock;
use std::sync::Mutex;

use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use rusqlite::Connection;

use crate::services::tps::install::{self, ReceiverConfig};
use crate::services::tps::model::{ReadBatch, SourceCursor, SourceState};
use crate::services::tps::sources::claude;
use crate::services::tps::store;
use crate::services::usage::error::UsageError;

/// 端口冲突 detail 的机器可定位前缀（sourceState / 错误面共用）
pub const PORT_CONFLICT_PREFIX: &str = "tps-receiver-port-conflict";

// ==== 端口选择（真实 bind 探测）====

/// 生产候选表：默认固定端口 + 顺延区间（47181 + 47182–47190；Spec §5 逐字）
pub fn default_port_candidates() -> Vec<u16> {
    let mut v = vec![install::DEFAULT_RECEIVER_PORT];
    v.extend(install::FALLBACK_PORT_RANGE);
    v
}

/// 对单个候选做**真实 bind 尝试**（127.0.0.1；成功即关——探测只验可绑定性）。
/// 返回实际绑定端口（候选 :0 时为 OS 分配值）。
pub fn try_bind_loopback(port: u16) -> Option<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).ok()?;
    let actual = listener.local_addr().ok()?.port();
    drop(listener);
    Some(actual)
}

/// 依序真实 bind 候选端口，取第一个可绑定的（全部被占 → None：调用方报状态，
/// **绝不**杀进程或静默换端口——Spec §5）。`probe_free_port()` 是生产入口（默认候选表）；
/// 测试用 [`probe_free_port_in`] 传临时候选，绝不 bind 47181–47190。
pub fn probe_free_port_in(candidates: &[u16]) -> Option<u16> {
    candidates.iter().find_map(|p| try_bind_loopback(*p))
}

/// 生产端口探测（默认候选表；只在「未持久化过接收器配置」的新装路径调用）
pub fn probe_free_port() -> Option<u16> {
    probe_free_port_in(&default_port_candidates())
}

/// 新装路径的接收器身份：**真实 bind 探测**选端口 + 新随机凭证（Task 5 已知缺口
/// 「preview 拿空占用表选端口」在本函数收口：持久化前端口必然真实试绑过）。
pub fn fresh_receiver_config() -> Result<ReceiverConfig, UsageError> {
    let port = probe_free_port().ok_or_else(|| {
        UsageError::new(
            "usage-source-io",
            format!(
                "tps-receiver-port-exhausted: receiver candidate ports {}-{} are all occupied; \
                 report status per Spec §5, never kill processes or switch ports silently",
                install::DEFAULT_RECEIVER_PORT,
                install::FALLBACK_PORT_RANGE.end()
            ),
        )
    })?;
    Ok(ReceiverConfig {
        port,
        credential: install::generate_receiver_credential(),
        created_at_ms: crate::services::usage::now_ms(),
    })
}

// ==== ingest 纯函数（连接注入；HTTP 层是薄壳）====

/// ingest 依赖缝：期望凭证 + 提交目标连接（测试传私有库，生产传全局 DB）
pub struct IngestDeps<'a> {
    pub expected_credential: &'a str,
    pub conn: &'a mut Connection,
}

/// ingest 结果（HTTP 状态码的语义面；测试直接断言本枚举）
#[derive(Debug, Clone, PartialEq)]
pub enum IngestOutcome {
    /// 已提交（观察数；诊断进 sourceState）
    Accepted {
        observations: usize,
        diagnostics: usize,
    },
    /// Bearer 缺失/方案不符/凭证不符
    Unauthorized,
    /// 超 1MiB（Content-Length 预检或流式截断命中）
    TooLarge,
    /// 非 OTLP JSON
    BadBody(String),
    /// 解码成功但落库失败（下一批靠稳定键 UPSERT 重收）
    StoreFailed(String),
}

/// 单条报文的完整守卫链 + 提交（Spec §5 边界逐条的可测形态）：
/// 1. Content-Length 预检（超限在**缓冲完整 body 前**拒绝）；
/// 2. body 实长复核（流式截断的兜底闸）；
/// 3. Bearer 常量时间比对；
/// 4. OTLP JSON 解码（解码层二次 1MiB 闸，纵深防御）；
/// 5. 观察 + 诊断 + 游标原位 同事务提交（claude 遥测按 trace+span 稳定键幂等，
///    游标无水位语义——原样回写）。
pub fn ingest(
    deps: &mut IngestDeps,
    auth_header: Option<&str>,
    content_length: Option<usize>,
    body: &[u8],
    _now_ms: i64,
) -> IngestOutcome {
    // 1. 头预检：声明超限直接拒绝（HTTP 层无需读 body 即可回 413）
    if let Some(len) = content_length {
        if install::enforce_body_limit(len).is_err() {
            return IngestOutcome::TooLarge;
        }
    }
    // 2. 实长复核（无 Content-Length / 声明不符时的流式闸）
    if install::enforce_body_limit(body.len()).is_err() {
        return IngestOutcome::TooLarge;
    }
    // 3. 凭证
    if install::verify_bearer(auth_header, deps.expected_credential).is_err() {
        return IngestOutcome::Unauthorized;
    }
    // 4. 解码（诊断随行）
    let (observations, diagnostics) = match claude::decode_trace_with_diagnostics(body) {
        Ok(pair) => pair,
        Err(e) => return IngestOutcome::BadBody(format!("{}: {}", e.code, e.detail)),
    };
    let n_diags = diagnostics.len();
    // 5. 同事务提交（诊断进 sourceState；游标原位回写——claude 无增量水位）
    let cursor = store::load_cursor(deps.conn, claude::SOURCE_ID)
        .ok()
        .flatten()
        .unwrap_or_else(|| SourceCursor::empty(claude::SOURCE_ID));
    let batch = ReadBatch {
        observations,
        source_state: SourceState {
            source: claude::SOURCE_ID.into(),
            cursor: cursor.clone(),
            ok: true,
            detail: bounded_detail(&diagnostics),
        },
        diagnostics,
        next_cursor: cursor,
    };
    let n_obs = batch.observations.len();
    match store::commit_batch(deps.conn, &batch) {
        Ok(()) => IngestOutcome::Accepted {
            observations: n_obs,
            diagnostics: n_diags,
        },
        Err(e) => IngestOutcome::StoreFailed(format!("{}: {}", e.code, e.detail)),
    }
}

/// 诊断串有界化（最多 8 条、总长 ≤500 字节——sourceState 是状态不是日志，不无限膨胀）。
/// 截断**字符边界安全**（评审 Important 2：按字节 truncate 在中文字符上 panic →
/// ingest 持续 503 → 确定性丢数据）。pub 供字符边界测试直测。
pub fn bounded_detail(diagnostics: &[String]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }
    let joined = diagnostics
        .iter()
        .take(8)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    if joined.len() <= MAX_DETAIL_BYTES {
        return Some(joined);
    }
    // 评审 Important 2：`String::truncate` 按字节切、非字符边界即 panic——本域诊断含
    // 中文（3 字节字符），panic 在 ingest 内 → spawn_blocking 转 JoinError → 持续 503；
    // 导出器对同一批确定性重发 = 诊断内容不变就永不提交（确定性丢数据）。改走
    // 「向下取字符边界」截断：只丢尾上不完整的那个字符。
    let cut = floor_char_boundary(joined.as_bytes(), MAX_DETAIL_BYTES);
    Some(joined[..cut].to_string())
}

/// detail 字节上限（sourceState 是状态不是日志，不无限膨胀）
const MAX_DETAIL_BYTES: usize = 500;

/// 字节上限的**向下**字符边界（floor char boundary；`str::floor_char_boundary` 仍是
/// nightly，手写同语义）：从 max 往回找 `is_char_boundary`，0 恒为边界故必收敛
pub fn floor_char_boundary(bytes: &[u8], max: usize) -> usize {
    let mut cut = max.min(bytes.len());
    // 字节位 cut 是边界 ⇔ bytes[cut] 不是 UTF-8 续字节（0b10xxxxxx）；cut == len 时
    // 末端恒为边界。向前跳过续字节即落到字符起点（0 恒为边界故必收敛）。
    while cut > 0 && cut < bytes.len() && (bytes[cut] & 0xC0) == 0x80 {
        cut -= 1;
    }
    cut
}

// ==== 端口冲突报状态 ====

/// 持久化端口被占时的绑定尝试（**只**试持久化端口——不换端口使旧配置失效）。
/// Err detail 带 [`PORT_CONFLICT_PREFIX`]，HTTP/日志/sourceState 共用同一句契约文案。
/// 成功返回的 listener **已是非阻塞**：`tokio::net::TcpListener::from_std` 要求非阻塞
/// 套接字（tokio 1.53 debug 构建对阻塞套接字直接 assert——实时路径启动即 panic 且
/// 守卫卡死；release 则反应器阻塞 accept），必须在进入 reactor 前置好。
pub fn bind_receiver_port(port: u16) -> Result<std::net::TcpListener, UsageError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!(
                "{PORT_CONFLICT_PREFIX}: persisted port {port} is occupied — report status per \
                 Spec §5, never kill processes or switch ports silently (bind error: {e})"
            ),
        )
    })?;
    listener.set_nonblocking(true).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!(
                "tps-receiver-listener-nonblocking: failed to set listener non-blocking before \
                 handing it to the reactor (io error: {e})"
            ),
        )
    })?;
    Ok(listener)
}

/// 端口冲突落状态：claude sourceState = ok=false + 冲突前缀（查询面「报状态」的落点）
pub fn record_port_conflict(conn: &Connection, config: &ReceiverConfig, now_ms: i64) {
    let _ = now_ms; // 状态行 updated_at 由 store::save_state 统一取当前时钟
    let state = SourceState {
        source: claude::SOURCE_ID.into(),
        cursor: SourceCursor::empty(claude::SOURCE_ID),
        ok: false,
        detail: Some(format!(
            "{PORT_CONFLICT_PREFIX}: persisted port {} is occupied — report status per Spec §5, \
             never kill processes or switch ports silently",
            config.port
        )),
    };
    if let Ok(v) = serde_json::to_value(&state) {
        if let Err(e) = store::save_state(conn, &store::source_state_key(claude::SOURCE_ID), &v) {
            log::warn!("tps/claude: 端口冲突状态写入失败：{}", e.detail);
        }
    }
}

// ==== HTTP 面（axum 薄壳；守卫逻辑全在 ingest 纯函数）====

/// 每请求从库加载接收器配置（重装/凭证变化自愈；单行读，1s 导出间隔下开销可忽略）
fn load_receiver_config() -> Result<ReceiverConfig, UsageError> {
    // install::load_receiver_config_at 自开短连接（安装域的私有库口径），此处不取全局 DB 锁
    install::load_receiver_config_at(&crate::services::tps::collector::runtime_home(), "claude")
        .and_then(|c| {
            c.ok_or_else(|| {
                UsageError::new(
                    "usage-source-io",
                    "tps-receiver-not-configured: local receiver not configured (claude statistics keys not installed)",
                )
            })
        })
}

async fn handle_ingest(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let content_length = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    // DB 访问出主线程（axum 任务在 tokio worker 上，但同步 rusqlite 调用显式下放）
    let body = body.to_vec();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let config = match load_receiver_config() {
            Ok(c) => c,
            Err(e) => return Err(format!("{}: {}", e.code, e.detail)),
        };
        let mut conn = crate::database::connection::DB
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut deps = IngestDeps {
            expected_credential: &config.credential,
            conn: &mut conn,
        };
        Ok(ingest(
            &mut deps,
            auth.as_deref(),
            content_length,
            &body,
            crate::services::usage::now_ms(),
        ))
    })
    .await
    .unwrap_or_else(|e| Err(format!("usage-internal: ingest task failed: {e}")));

    match result {
        Ok(IngestOutcome::Accepted { .. }) => {
            (StatusCode::OK, axum::Json(serde_json::json!({}))).into_response()
        }
        Ok(IngestOutcome::Unauthorized) => {
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
        }
        Ok(IngestOutcome::TooLarge) => {
            (StatusCode::PAYLOAD_TOO_LARGE, "payload too large").into_response()
        }
        Ok(IngestOutcome::BadBody(d)) => (StatusCode::BAD_REQUEST, d).into_response(),
        Ok(IngestOutcome::StoreFailed(d)) => (StatusCode::INTERNAL_SERVER_ERROR, d).into_response(),
        Err(d) => (StatusCode::SERVICE_UNAVAILABLE, d).into_response(),
    }
}

/// 接收器路由（只挂 `/v1/traces` POST；其余路径 axum 默认 404——「仅 JSON、仅 traces」）
pub fn router() -> Router {
    Router::new()
        .route(install::INGEST_PATH, post(handle_ingest))
        .layer(DefaultBodyLimit::max(install::MAX_BODY_BYTES))
}

// ==== 生命周期（随总授权启停；进程退出即停）====

/// 运行中的接收器句柄：**代际号**（退出路径只清自己那代，防迟退老任务误清新句柄）
/// + 优雅停机信号
struct ReceiverHandle {
    generation: u64,
    shutdown: tokio::sync::watch::Sender<bool>,
}

static RECEIVER: LazyLock<Mutex<Option<ReceiverHandle>>> = LazyLock::new(|| Mutex::new(None));

static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 上次「尝试启动」时刻：失败路径（未配置/端口冲突）**不每秒重试**——运行拍是 1 秒
/// 节奏，无节流会把「开库连接 + bind + 冲突状态写」打成每秒风暴（自审追记）。
/// 初始 0（不是 i64::MIN：`now - MIN` 会溢出，debug 构建即 panic——修正轮追记）。
static LAST_START_ATTEMPT_MS: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(0));
/// 失败重试节流窗
const START_RETRY_MS: i64 = 15_000;

/// 确保接收器在跑（幂等）：守卫短路 → 节流 → 读持久化配置 → **只 bind 持久化端口**
/// → 交 [`spawn_serve`] 启动。冲突 = 报状态（sourceState ok=false + 日志），不启动也
/// 不换端口。未配置（未安装 claude 统计键）→ 不启动（静默，15 秒后重查——装完统计键
/// 自然接上）。
pub fn ensure_started() {
    {
        let guard = RECEIVER.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return; // 已在跑（幂等短路；绝不对在跑的持久化端口重复 bind）
        }
    }
    let now = crate::services::usage::now_ms();
    {
        let mut last = LAST_START_ATTEMPT_MS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if now - *last < START_RETRY_MS {
            return;
        }
        *last = now;
    }
    let config = match load_receiver_config() {
        Ok(c) => c,
        Err(_) => return, // 未配置：静默不启动（未装 claude 统计键是正常态）
    };
    let std_listener = match bind_receiver_port(config.port) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("tps/claude: {}", e.detail);
            // 冲突报状态——先确认不是罕见竞态（守卫刚被并发启动占住 = 端口是我们
            // 自己的，不写冲突状态误报）
            let self_held = RECEIVER.lock().unwrap_or_else(|e| e.into_inner()).is_some();
            if !self_held {
                let conn = crate::database::connection::DB
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                record_port_conflict(&conn, &config, now);
            }
            return;
        }
    };
    if spawn_serve(std_listener).is_none() {
        log::debug!("tps/claude: 接收器已由并发路径启动，丢弃重复 listener");
    }
}

/// 启动 axum serve 任务（生产 [`ensure_started`] 与**实时路径测试**共用的唯一入口）：
/// 注册本代守卫 → spawn（非阻塞 listener 经 `from_std` 进 reactor → serve → **退出
/// 必清本代守卫**）。守卫已被占 → None（调用方负责丢弃未注册的 listener）。
/// 返回 JoinHandle 供测试等待任务退出（生产侧 detach，进程生命周期兜底）。
pub fn spawn_serve(
    std_listener: std::net::TcpListener,
) -> Option<tauri::async_runtime::JoinHandle<()>> {
    let mut guard = RECEIVER.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return None; // 已在跑：守卫被占，调用方丢弃 listener
    }
    let generation = NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (tx, rx) = tokio::sync::watch::channel(false);
    let app = router();
    *guard = Some(ReceiverHandle {
        generation,
        shutdown: tx,
    });
    let join = tauri::async_runtime::spawn(async move {
        // —— 实时服务路径（评审 Important 1 的修复面：非阻塞 listener 才能 from_std）——
        let outcome: Result<u16, String> = async {
            let listener = tokio::net::TcpListener::from_std(std_listener)
                .map_err(|e| format!("tps-receiver-listener-convert: from_std failed: {e}"))?;
            let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
            // 随 Tuvis 退出 / 总授权关闭而停（watch 关闭即优雅停机）。
            // 必须真的 .await 这个 future（`let _ =` 不会轮询 → 服务器永不启动）。
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let mut rx = rx;
                    let _ = rx.changed().await;
                })
                .await
                .map_err(|e| format!("tps-receiver-serve: axum serve failed: {e}"))?;
            Ok(port)
        }
        .await;
        match outcome {
            Ok(port) => log::info!("tps/claude: 接收器已停止（端口 {port}）"),
            Err(e) => log::warn!("tps/claude: 接收器任务异常退出：{e}"),
        }
        // 无论正常停机、任务死亡还是 from_std 失败——清掉本代守卫，运行拍即可重试
        //（评审 Important 1：守卫卡死 = 静默死掉的接收器，无重试也无冲突状态）
        clear_guard_if_current(generation);
    });
    Some(join)
}

/// 只清**本代**守卫：迟退的老任务不得误清新启动的句柄
fn clear_guard_if_current(generation: u64) {
    let mut guard = RECEIVER.lock().unwrap_or_else(|e| e.into_inner());
    if guard.as_ref().is_some_and(|h| h.generation == generation) {
        *guard = None;
    }
}

/// 停止接收器（总授权关闭 / Tuvis 退出清理调用；幂等）
pub fn shutdown() {
    let mut guard = RECEIVER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = guard.take() {
        let _ = h.shutdown.send(true);
    }
}

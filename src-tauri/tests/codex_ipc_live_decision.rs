//! #172 T10-A：真机「审批决策闭环」**受控探针**（`#[ignore]`、需用户在场 + 显式授权）。
//!
//! 目标：对一条**挂起的命令/文件审批**发送**一次**决策（`accept` 或 `cancel`，由环境变量
//! 指定），把「pending → 决策请求 → 对端回执」三段**脱敏**成一份夹具写到 `%TEMP%`，
//! 供人工审阅后入仓（`tests/fixtures/codex-desktop-ipc/observed/`）。
//!
//! 硬纪律（施工单 §3）：
//! * **显式同意闸**：`TUVIS_LIVE_DECISION=1` + `TUVIS_CAPTURE_THREAD=<thread-uuid>` +
//!   `TUVIS_DECISION=accept|cancel` 三者齐备才动手；缺任一项 → 立即退出并打印原因
//!   （缺 thread id 时按既有 `candidate_threads` 逻辑列出最近候选，**不自己挑**）。
//! * **只对命令/文件审批应答**：`item/commandExecution/requestApproval` /
//!   `item/fileChange/requestApproval` 之外的 pending（`item/plan/requestImplementation`、
//!   `item/permissions/requestApproval`、`item/tool/requestUserInput` …）一律**拒绝发帧**
//!   并退出（打印该 method）——绝不替用户做计划实现、权限升格或问答。
//! * **只发一次**：决策帧只发一次；拿到回执（或超时）即停，随后 `following:false` +
//!   关闭连接。**不重试、不对同一 requestId 发第二次**。
//! * **不碰仓库**：只写 `%TEMP%`；夹具由人工审阅后再入仓。
//! * **如实报告**：未发现 pending / owner 不可达 / 无回执 → 打印明确原因并退出
//!   （不重试、不猜）。
//!
//! **本探针故意不走 capability 门**（产品路径的 `CapabilitySet` 闸）：本仓 shipped 表
//! 尚未登记 build `26.1016.6573.0`，而 T10-A 的目的正是**取这条 build 的决策闭环证据**。
//! 代替 capability 门的是**显式同意闸**（用户在场 + 三个环境变量）。
//!
//! 运行方式（`src-tauri/` 下，需用户在真机 Desktop 前在场）：
//!
//! ```powershell
//! $env:TUVIS_LIVE_DECISION = "1"
//! $env:TUVIS_CAPTURE_THREAD = "<thread-uuid>"
//! $env:TUVIS_DECISION = "accept"        # 或 "cancel"
//! cargo test --features marker-helper --test codex_ipc_live_decision -- --ignored --nocapture
//! ```
//!
//! 建议顺序：先在 Desktop 里让 Codex 发起一条**需要审批的命令**（例如 `echo hi`）并停在
//! 审批框上，再跑本探针——它会在等待窗口里自动抓下第一条 pending 并**只发一次**决策。
//!
//! 自测（同文件的普通 `#[test]`，**不连真机**）：用本地假 peer（duplex）验证
//! ① 缺 `TUVIS_LIVE_DECISION=1` → 零帧；② plan pending → 拒绝发帧（零决策帧）；
//! ③ 命令审批 + `cancel` → 只发一帧且方法/params 正确、随后 unfollow；
//! ④ 脱敏输出不含命令正文原文。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::{
    Connector, ProductionConnector,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::codec::{encode_frame, FrameDecoder};
use multi_agents_manager_lib::services::codex_desktop_ipc::connection::{
    spawn, ConnectionEvent, ConnectionHandle, RpcRequest,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::owners::{
    declare_following, discover_owner,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::{
    profile_for, ProtocolProfile,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::{
    BoxIo, DesktopEndpoint, EndpointResolver, IpcIo,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::types::{
    Delivery, OwnerLease, PendingRequestId, ProtocolError, ThreadKey,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// 真机探针共用件（脱敏口径 / 会话候选枚举 / following 助手）——与
/// `codex_ipc_live_capture.rs` 共用同一份实现，避免两处夹具脱敏强度漂移。
#[path = "support/codex_live.rs"]
mod codex_live;
use codex_live::{candidate_threads, sanitize, KEEP_STRINGS, MAX_ARRAY, MAX_OBJECT_KEYS};

/// 环境变量名（**显式同意闸**）。
const ENV_AUTHORIZE: &str = "TUVIS_LIVE_DECISION";
const ENV_THREAD: &str = "TUVIS_CAPTURE_THREAD";
const ENV_DECISION: &str = "TUVIS_DECISION";
const ENV_LABEL: &str = "TUVIS_DECISION_LABEL";
const ENV_WAIT_SECS: &str = "TUVIS_DECISION_WAIT_SECS";

/// 决策词白名单：只允许 `accept` / `cancel`（其它值一律退出，**不自造决策词**）。
const DECISION_ACCEPT: &str = "accept";
const DECISION_CANCEL: &str = "cancel";

/// 命令审批的决策方法（官方方法表登记 v1）。
const METHOD_COMMAND_DECISION: &str = "thread-follower-command-approval-decision";
/// 文件变更审批的决策方法（官方方法表登记 v1，与 command 并列、互不替代）。
const METHOD_FILE_DECISION: &str = "thread-follower-file-approval-decision";

/// 脱敏占位符（写盘时**不写真实** threadId/turnId/itemId/envelope UUID/client id）。
const PLACEHOLDER_THREAD_UUID: &str = "00000000-0000-4000-8000-000000000001";
const PLACEHOLDER_TURN_UUID: &str = "00000000-0000-4000-8000-000000000002";
const PLACEHOLDER_ITEM_ID: &str = "exec-000000000003";
const PLACEHOLDER_ENVELOPE_UUID: &str = "00000000-0000-4000-8000-000000000004";
const PLACEHOLDER_CLIENT_UUID: &str = "00000000-0000-4000-8000-000000000005";

// ---------------------------------------------------------------- 工具入参

/// 工具入参（从环境变量读；自测可注入假值，**不依赖进程环境**）。
#[derive(Debug, Default, Clone)]
struct ToolEnv {
    /// `TUVIS_LIVE_DECISION`——用户显式授权（必须为 `1`）。
    authorized: Option<String>,
    /// `TUVIS_CAPTURE_THREAD`——要操作的会话 id。
    thread: Option<String>,
    /// `TUVIS_DECISION`——决策词（`accept` / `cancel`）。
    decision: Option<String>,
    /// `TUVIS_DECISION_LABEL`——输出文件名标签（可选）。
    label: Option<String>,
    /// `TUVIS_DECISION_WAIT_SECS`——等待挂起请求的秒数（可选，默认 60）。
    wait_secs: Option<String>,
}

impl ToolEnv {
    /// 从**进程环境**读取（真机路径；自测不走这里）。
    fn from_process() -> Self {
        Self {
            authorized: std::env::var(ENV_AUTHORIZE).ok(),
            thread: std::env::var(ENV_THREAD).ok(),
            decision: std::env::var(ENV_DECISION).ok(),
            label: std::env::var(ENV_LABEL).ok(),
            wait_secs: std::env::var(ENV_WAIT_SECS).ok(),
        }
    }

    /// 等待窗口（秒）。
    fn wait_seconds(&self) -> u64 {
        self.wait_secs
            .as_deref()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(60)
    }

    /// 输出标签（缺省 `live-decision`）。
    fn label(&self) -> &str {
        self.label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("live-decision")
    }
}

// ---------------------------------------------------------------- 显式同意闸

/// 闸门失败的原因类别（自测按类别断言，不依赖文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateErrorKind {
    /// 缺 `TUVIS_LIVE_DECISION=1`（用户未显式授权）。
    MissingAuthorization,
    /// 缺 `TUVIS_CAPTURE_THREAD`（不知道操作哪条会话；**不自己挑**）。
    MissingThread,
    /// `TUVIS_DECISION` 缺失或不是 `accept|cancel`。
    InvalidDecision,
}

/// 闸门失败（带人类可读原因）。
#[derive(Debug)]
struct GateError {
    kind: GateErrorKind,
    message: String,
}

/// 闸门通过后拿到的操作凭据。
#[derive(Debug, Clone, PartialEq)]
struct Gate {
    thread_id: String,
    decision: &'static str,
}

/// 决策词解析：只接受 `accept` / `cancel`。
fn parse_decision(raw: &str) -> Result<&'static str, String> {
    match raw.trim() {
        DECISION_ACCEPT => Ok(DECISION_ACCEPT),
        DECISION_CANCEL => Ok(DECISION_CANCEL),
        other => Err(format!(
            "{ENV_DECISION} 只允许 {DECISION_ACCEPT}|{DECISION_CANCEL}，收到 {other:?}"
        )),
    }
}

/// **显式同意闸**：三项齐备才动手；否则返回原因（调用方立即退出，**零帧**）。
fn check_gate(env: &ToolEnv) -> Result<Gate, GateError> {
    // ① 用户显式授权
    if env.authorized.as_deref().map(str::trim) != Some("1") {
        return Err(GateError {
            kind: GateErrorKind::MissingAuthorization,
            message: format!("未设置 {ENV_AUTHORIZE}=1（用户显式授权）——拒绝动手（零帧）"),
        });
    }
    // ② 要操作的会话 id（缺失时由调用方列候选，不自己挑）
    let thread_id = env.thread.as_deref().unwrap_or("").trim().to_string();
    if thread_id.is_empty() {
        return Err(GateError {
            kind: GateErrorKind::MissingThread,
            message: format!("未设置 {ENV_THREAD}（要操作的会话 id）——拒绝动手（零帧）"),
        });
    }
    // ③ 决策词（只允许 accept / cancel）
    let decision =
        parse_decision(env.decision.as_deref().unwrap_or("")).map_err(|message| GateError {
            kind: GateErrorKind::InvalidDecision,
            message,
        })?;
    Ok(Gate {
        thread_id,
        decision,
    })
}

// ---------------------------------------------------------------- pending 分类

/// pending method → 决策方法名。**只认命令/文件审批**；其余一律 `None`（拒绝发帧）。
fn decision_method_for(method: &str) -> Option<&'static str> {
    match method {
        "item/commandExecution/requestApproval" => Some(METHOD_COMMAND_DECISION),
        "item/fileChange/requestApproval" => Some(METHOD_FILE_DECISION),
        _ => None,
    }
}

/// pending 自带的 `availableDecisions` **字符串词**（对象形态 = 执行策略修订 =
/// 权限扩大，**永不发**）。
fn offered_decision_words(params: &serde_json::Value) -> Vec<String> {
    params
        .get("availableDecisions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- 决策闭环

/// 决策闭环失败。
enum FlowError {
    /// pending method 不支持 → **拒绝发帧**（零决策帧）。
    Refused(String),
    /// 环境/连接/等待失败。
    Failed(String),
}

/// 对端回执形态。
#[derive(Debug, Clone)]
struct Receipt {
    /// `success` / `error` / `none`。
    result_type: String,
    /// 回执是否携带 `handledByClientId`。
    handled_by_present: bool,
    /// 回执 result（已脱敏）。
    result: Option<serde_json::Value>,
    /// 无回执原因（`result_type == "none"` 时有值）。
    note: Option<String>,
}

impl Receipt {
    /// 控制台摘要（不含任何真实 ID / 正文）。
    fn describe(&self) -> String {
        match (self.result_type.as_str(), &self.note) {
            ("none", Some(note)) => format!("无回执（{note}）"),
            (result_type, _) => {
                let handled = if self.handled_by_present {
                    "存在"
                } else {
                    "不存在"
                };
                let result = match &self.result {
                    Some(v) => v.to_string(),
                    None => "无".to_string(),
                };
                format!("resultType={result_type}；handledByClientId={handled}；result={result}")
            }
        }
    }
}

/// 决策闭环产物（写夹具的原料；**原始值只在内存里，落盘前一律脱敏**）。
struct DecisionOutcome {
    /// 原始 pending 请求条目。
    pending: serde_json::Value,
    /// pending method（协议词汇，原值可写）。
    pending_method: String,
    /// pending id 的 JSON 类型（`number` / `string`）。
    id_kind: &'static str,
    /// 决策方法名。
    decision_method: &'static str,
    /// 决策词。
    decision: &'static str,
    /// 方法版本（**从方法表取**）。
    version: u32,
    /// 决策帧 params（原始值——落盘时按占位规则重建，不直接 dump）。
    params: serde_json::Value,
    /// 对端回执（`result_type == "none"` 表示无回执，`note` 说明原因）。
    receipt: Option<Receipt>,
}

/// 从 snapshot 帧里取第一条 pending 请求（只认 `snapshot` 型 change）。
fn first_pending(frame: &serde_json::Value) -> Option<serde_json::Value> {
    let change = frame.get("params")?.get("change")?;
    if change.get("type").and_then(|t| t.as_str()) != Some("snapshot") {
        return None;
    }
    let requests = change
        .get("conversationState")?
        .get("requests")?
        .as_array()?;
    requests.first().cloned()
}

/// 等一条挂起请求（`conversationState.requests` 非空的首帧）。
async fn wait_for_pending(
    conn: &ConnectionHandle,
    rx: &mut tokio::sync::broadcast::Receiver<ConnectionEvent>,
    key: &ThreadKey,
    wait_secs: u64,
) -> Result<serde_json::Value, FlowError> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left.min(Duration::from_secs(5)), rx.recv()).await {
            Ok(Ok(ConnectionEvent::Broadcast { frame, .. })) => {
                if frame.get("method").and_then(|m| m.as_str())
                    == Some("thread-stream-state-changed")
                    && frame["params"]["conversationId"]
                        == serde_json::Value::String(key.thread_id.clone())
                {
                    if let Some(pending) = first_pending(&frame) {
                        return Ok(pending);
                    }
                    // 静默/空 requests：再要一次首帧（Desktop 只在状态变化时推送）
                    let _ = codex_live::status_requested(conn, key).await;
                }
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => {
                return Err(FlowError::Failed("连接事件通道已关闭".to_string()));
            }
            Err(_) => continue, // 分片超时 → 回到循环重判总时限
        }
    }
    Err(FlowError::Failed(format!(
        "等待 {wait_secs}s 未发现挂起的审批请求（requests 一直为空）"
    )))
}

/// 决策闭环：discovery → following → 等 pending → **只发一次**决策帧 → 等回执。
async fn run_decision_flow(
    conn: &ConnectionHandle,
    key: &ThreadKey,
    decision: &'static str,
    wait_secs: u64,
) -> Result<DecisionOutcome, FlowError> {
    // ① owner discovery（谁持有这条会话）
    let owner = discover_owner(conn, key).await.map_err(|e| {
        FlowError::Failed(format!(
            "owner discovery 失败（{e}）——该线程是否由 Desktop 持有？"
        ))
    })?;
    // ② 声明 following（订阅；只读）
    let mut rx = conn.subscribe();
    declare_following(conn, key)
        .await
        .map_err(|e| FlowError::Failed(format!("声明 following 失败：{e}")))?;
    let _ = codex_live::status_requested(conn, key).await;
    eprintln!(
        "已声明 following（owner 已发现），等待「requests 非空」的 snapshot（最多 {wait_secs}s）……\n\
         现在请在 Desktop 里让 Codex 发起一条**需要审批的命令**（例如 echo hi），并停在审批框上。"
    );
    // ③ 等 pending
    let pending = wait_for_pending(conn, &mut rx, key, wait_secs).await?;

    // ④ 只对命令/文件审批应答（施工单 §3：其它 method 一律拒绝发帧）
    let pending_method = pending
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let Some(decision_method) = decision_method_for(&pending_method) else {
        return Err(FlowError::Refused(format!(
            "pending method={pending_method} 不是命令/文件审批——按施工单 §3 **拒绝发帧**（零决策帧）"
        )));
    };
    // ⑤ pending id（不透明标识：整数或字符串；浮点等非法形态拒绝）
    let raw_id = pending.get("id").cloned().ok_or_else(|| {
        FlowError::Refused("pending 没有 id 字段——无法组装 requestId".to_string())
    })?;
    let request_id: PendingRequestId = serde_json::from_value(raw_id)
        .map_err(|e| FlowError::Refused(format!("pending id 非法（只允许整数/字符串）：{e}")))?;
    let id_kind = match &request_id {
        PendingRequestId::String(_) => "string",
        PendingRequestId::Integer(_) => "number",
    };
    let id_json = serde_json::to_value(&request_id).unwrap_or(serde_json::Value::Null);
    // ⑥ 决策词必须是服务端给出的词（availableDecisions 字符串词）——不猜
    let pending_params = pending
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let offered = offered_decision_words(&pending_params);
    if !offered.is_empty() && !offered.iter().any(|w| w == decision) {
        return Err(FlowError::Refused(format!(
            "pending 的 availableDecisions 字符串词是 {offered:?}，不含 {decision:?}——**拒绝发帧**（不猜）"
        )));
    }
    // ⑦ 决策帧组装（params 与本仓实现一致：conversationId / requestId / decision）
    let params = serde_json::json!({
        "conversationId": key.thread_id,
        "requestId": id_json,
        "decision": decision,
    });
    // ⑧ 版本**从方法表取**（`conn.version_of`），不硬编码
    let version = conn.version_of(decision_method).map_err(|e| {
        FlowError::Failed(format!(
            "方法 {decision_method} 不在版本表（{e}）——拒绝盲发"
        ))
    })?;
    // ⑨ 目标 owner（信封级 targetClientId）
    let lease = OwnerLease {
        key: key.clone(),
        client_id: owner,
        epoch: conn.epoch(),
        generation: 1,
    };
    // ⑩ **只发一次**：单次 RPC；不重试、不对同一 requestId 发第二次
    let outcome = conn
        .request(RpcRequest {
            method: decision_method.to_string(),
            params: params.clone(),
            version: Some(version),
            target: Some(lease),
            mutating: true,
            validity: None,
            source_client_id: None,
            timeout_ms: None,
        })
        .await;
    let receipt = match outcome.delivery {
        Delivery::Accepted => Receipt {
            result_type: "success".to_string(),
            handled_by_present: outcome.handled_by.is_some(),
            result: outcome.result.as_ref().map(|r| sanitize(r, Some("result"))),
            note: None,
        },
        Delivery::Rejected => Receipt {
            result_type: "error".to_string(),
            handled_by_present: outcome.handled_by.is_some(),
            result: outcome.result.as_ref().map(|r| sanitize(r, Some("result"))),
            note: None,
        },
        Delivery::NotSent => Receipt {
            result_type: "none".to_string(),
            handled_by_present: false,
            result: None,
            note: Some(format!(
                "零投递（帧未写出）：{}",
                outcome.reason_code.unwrap_or_else(|| "未知".to_string())
            )),
        },
        Delivery::UnknownDelivery => Receipt {
            result_type: "none".to_string(),
            handled_by_present: false,
            result: None,
            note: Some(format!(
                "无回执（投递结果未知，不重试）：{}",
                outcome.reason_code.unwrap_or_else(|| "未知".to_string())
            )),
        },
    };
    Ok(DecisionOutcome {
        pending,
        pending_method,
        id_kind,
        decision_method,
        decision,
        version,
        params,
        receipt: Some(receipt),
    })
}

// ---------------------------------------------------------------- 脱敏与夹具

/// pending 专用脱敏：协议词汇原值；标识符/正文/路径/理由/时间戳占位。
///
/// 口径 = 共用模块的 `sanitize`（同一份 `KEEP_STRINGS` 白名单）**加上** pending 专用
/// 占位规则（命令正文、cwd、reason、startedAtMs、threadId/turnId/itemId）。
fn sanitize_pending_value(value: &serde_json::Value, key: Option<&str>) -> serde_json::Value {
    // 键级占位规则（**与值的 JSON 类型无关**）：startedAtMs 是数字，不能只按字符串处理。
    if let Some(k) = key {
        if k == "startedAtMs" {
            return serde_json::json!("<redacted>");
        }
    }
    match value {
        serde_json::Value::String(s) => {
            if let Some(k) = key {
                match k {
                    "command" => return serde_json::json!("<synthetic command>"),
                    "cwd" => return serde_json::json!("<synthetic cwd>"),
                    "reason" => return serde_json::json!("<redacted>"),
                    "threadId" => return serde_json::json!(PLACEHOLDER_THREAD_UUID),
                    "conversationId" => return serde_json::json!(PLACEHOLDER_THREAD_UUID),
                    "turnId" => return serde_json::json!(PLACEHOLDER_TURN_UUID),
                    "itemId" => return serde_json::json!(PLACEHOLDER_ITEM_ID),
                    _ => {}
                }
                if KEEP_STRINGS.contains(&k) {
                    return serde_json::Value::String(s.clone());
                }
            }
            serde_json::json!(format!("<redacted:{}>", s.chars().count()))
        }
        serde_json::Value::Number(n) => serde_json::Value::Number(n.clone()),
        serde_json::Value::Bool(b) => serde_json::Value::Bool(*b),
        serde_json::Value::Null => serde_json::Value::Null,
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .take(MAX_ARRAY)
                .map(|v| sanitize_pending_value(v, key))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map.iter().take(MAX_OBJECT_KEYS) {
                out.insert(k.clone(), sanitize_pending_value(v, Some(k)));
            }
            serde_json::Value::Object(out)
        }
    }
}

/// pending 条目脱敏：`method` 原值；`id` 保留 JSON 类型（整数是计数器，原值；字符串用占位）。
fn sanitize_pending_entry(pending: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let Some(m) = pending.get("method") {
        out.insert("method".to_string(), m.clone());
    }
    match pending.get("id") {
        Some(serde_json::Value::Number(n)) => {
            out.insert("id".to_string(), serde_json::Value::Number(n.clone()));
        }
        Some(serde_json::Value::Null) => {
            out.insert("id".to_string(), serde_json::Value::Null);
        }
        Some(_) => {
            out.insert("id".to_string(), serde_json::json!("<synthetic id>"));
        }
        None => {}
    }
    if let Some(p) = pending.get("params") {
        out.insert("params".to_string(), sanitize_pending_params(p));
    }
    serde_json::Value::Object(out)
}

/// pending params 脱敏：`availableDecisions` 走共用 `sanitize`（**形状**保留：字符串词
/// 原样 + 对象形态内部占位）；其余按 pending 专用规则。
fn sanitize_pending_params(params: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(map) = params else {
        return sanitize(params, Some("params"));
    };
    let mut out = serde_json::Map::new();
    for (k, v) in map.iter().take(MAX_OBJECT_KEYS) {
        let sanitized = if k == "availableDecisions" {
            sanitize(v, Some("availableDecisions"))
        } else {
            sanitize_pending_value(v, Some(k))
        };
        out.insert(k.clone(), sanitized);
    }
    serde_json::Value::Object(out)
}

/// pending id 的脱敏形态（整数原值 / 字符串占位）。
fn sanitize_request_id(id: &serde_json::Value) -> serde_json::Value {
    match id {
        serde_json::Value::Number(n) => serde_json::Value::Number(n.clone()),
        serde_json::Value::String(_) => serde_json::json!("<synthetic id>"),
        other => sanitize(other, Some("requestId")),
    }
}

/// 组装脱敏夹具（三段：`pendingRequest` / `request` / `response`）。
fn build_fixture(outcome: &DecisionOutcome, label: &str) -> serde_json::Value {
    let request_id = outcome
        .params
        .get("requestId")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let response = match &outcome.receipt {
        Some(receipt) => {
            let mut map = serde_json::Map::new();
            map.insert("type".to_string(), serde_json::json!("response"));
            map.insert(
                "requestId".to_string(),
                serde_json::json!(PLACEHOLDER_ENVELOPE_UUID),
            );
            map.insert(
                "resultType".to_string(),
                serde_json::json!(receipt.result_type.clone()),
            );
            map.insert(
                "method".to_string(),
                serde_json::json!(outcome.decision_method),
            );
            // handledByClientId 是否存在（存在才写真值位——统一写占位 UUID）
            map.insert(
                "handledByClientId".to_string(),
                if receipt.handled_by_present {
                    serde_json::json!(PLACEHOLDER_CLIENT_UUID)
                } else {
                    serde_json::Value::Null
                },
            );
            match (&receipt.result, &receipt.note) {
                (Some(result), _) => {
                    map.insert("result".to_string(), result.clone());
                }
                (None, Some(note)) => {
                    map.insert("_note".to_string(), serde_json::json!(note));
                }
                (None, None) => {}
            }
            serde_json::Value::Object(map)
        }
        None => serde_json::json!({ "resultType": "none", "_note": "无回执" }),
    };
    serde_json::json!({
        "_source": format!(
            "真机审批决策闭环探针（T10-A；{label}）；命令正文/路径/标识符已占位化，仅保留结构性事实"
        ),
        "pendingRequest": sanitize_pending_entry(&outcome.pending),
        "request": {
            "type": "request",
            "requestId": PLACEHOLDER_ENVELOPE_UUID,
            "method": outcome.decision_method,
            "params": {
                "conversationId": PLACEHOLDER_THREAD_UUID,
                "requestId": sanitize_request_id(&request_id),
                "decision": outcome.decision,
            },
            "version": outcome.version,
            "targetClientId": PLACEHOLDER_CLIENT_UUID,
        },
        "response": response,
    })
}

// ---------------------------------------------------------------- 顶层执行

/// 顶层执行失败。
#[derive(Debug)]
enum ToolError {
    /// 闸门未通过（**零帧**）。
    Gate(GateError),
    /// pending 不支持 → 拒绝发帧（零决策帧）。
    Refused(String),
    /// 连接/等待/落盘失败。
    Failed(String),
}

/// 顶层执行产物。
#[derive(Debug)]
struct ToolOutcome {
    pending_method: String,
    id_kind: &'static str,
    decision_method: &'static str,
    decision: &'static str,
    version: u32,
    receipt: Option<Receipt>,
    fixture: serde_json::Value,
    out_path: Option<PathBuf>,
}

/// 顶层执行：**闸门 → 连接 → 决策闭环 → 落盘**。
///
/// `out_path = None` 时只产出结果、不写文件（自测用）。闸门不过 → **不连接、不发帧**。
async fn execute(
    connector: &dyn Connector,
    env: &ToolEnv,
    out_path: Option<PathBuf>,
) -> Result<ToolOutcome, ToolError> {
    // ① 显式同意闸（**先于一切 I/O**：不过则零帧）
    let gate = check_gate(env).map_err(ToolError::Gate)?;
    // ② 连接（生产 = Windows named pipe / macOS unix socket；含服务端身份校验）
    let (io, endpoint) = connector.connect_endpoint().await.map_err(|e| {
        ToolError::Failed(format!(
            "连接 Desktop IPC 失败（Desktop 没起 / 管道身份校验失败）：{e}"
        ))
    })?;
    let build = endpoint.build.clone().unwrap_or_default();
    // 未登记 build → 证据档未命中：方法表仍在（版本仍从表取），能力位全关——
    // 本探针**故意**不走 capability 门（见文件头说明），由显式同意闸把守。
    let profile = profile_for(&endpoint.platform, &build)
        .unwrap_or_else(|| ProtocolProfile::unverified(&endpoint.platform, &build));
    let conn = spawn(io, profile, 1);
    eprintln!(
        "已连接：platform={} build={}（本探针不走 capability 门；显式同意闸已放行）",
        endpoint.platform, build
    );
    // ③ 握手（initialize 由连接 actor 内部完成）
    let init_deadline = Instant::now() + Duration::from_secs(5);
    while conn.initialized_client().is_none() {
        if Instant::now() >= init_deadline {
            conn.shutdown().await;
            return Err(ToolError::Failed("initialize 超时（5s）".to_string()));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let key = ThreadKey {
        host_id: "local".to_string(),
        thread_id: gate.thread_id.clone(),
    };
    // ④ 决策闭环（只发一次）
    let flow = run_decision_flow(&conn, &key, gate.decision, env.wait_seconds()).await;
    // ⑤ 撤销订阅 + 关闭连接（无论成败）。
    // broadcast 是 fire-and-forget：给 writer 一点刷写时间，否则 shutdown 的旁路信号会
    // 抢先收尾、把这条 following:false 丢掉（Desktop 侧留着一条陈旧订阅）。
    let _ = codex_live::unfollow(&conn, &key).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    conn.shutdown().await;
    let outcome = flow.map_err(|e| match e {
        FlowError::Refused(message) => ToolError::Refused(message),
        FlowError::Failed(message) => ToolError::Failed(message),
    })?;
    // ⑥ 落盘（脱敏夹具；只写 %TEMP%，不碰仓库）
    let fixture = build_fixture(&outcome, env.label());
    if let Some(path) = &out_path {
        std::fs::write(
            path,
            serde_json::to_string_pretty(&fixture).expect("序列化夹具"),
        )
        .map_err(|e| ToolError::Failed(format!("写输出文件失败：{e}")))?;
    }
    Ok(ToolOutcome {
        pending_method: outcome.pending_method,
        id_kind: outcome.id_kind,
        decision_method: outcome.decision_method,
        decision: outcome.decision,
        version: outcome.version,
        receipt: outcome.receipt,
        fixture,
        out_path,
    })
}

/// 控制台摘要：**不打印**命令正文、路径、ID 原文。
fn print_summary(outcome: &ToolOutcome) {
    println!("pending method = {}", outcome.pending_method);
    println!("pending id 类型 = {}", outcome.id_kind);
    println!(
        "已发决策 = {}（version={}，decision={}）",
        outcome.decision_method, outcome.version, outcome.decision
    );
    println!(
        "对端回执 = {}",
        outcome
            .receipt
            .as_ref()
            .map(Receipt::describe)
            .unwrap_or_else(|| "无回执".to_string())
    );
    if let Some(path) = &outcome.out_path {
        println!("输出文件 = {}", path.display());
    }
}

// ---------------------------------------------------------------- 真机入口

/// 真机「审批决策闭环」探针（**`#[ignore]`**：需用户在场 + 显式同意闸）。
#[tokio::test]
#[ignore = "真机决策闭环探针：需用户在场 + TUVIS_LIVE_DECISION=1 显式授权；用 --ignored 显式运行"]
async fn live_decision_closure() {
    let env = ToolEnv::from_process();
    // 生产 endpoint（Windows named pipe / macOS unix socket；含服务端身份校验）。
    // 按 target_os 分开构造：`UnixEndpointResolver` 的 `legacy_tmpdir` 字段只在 unix 存在。
    #[cfg(target_os = "macos")]
    let resolver: Arc<dyn EndpointResolver> = Arc::new(
        multi_agents_manager_lib::services::codex_desktop_ipc::transport::unix::UnixEndpointResolver {
            codex_home: None,
            legacy_tmpdir: None,
        },
    );
    #[cfg(target_os = "windows")]
    let resolver: Arc<dyn EndpointResolver> = Arc::new(
        multi_agents_manager_lib::services::codex_desktop_ipc::transport::windows::WindowsEndpointResolver,
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let resolver: Arc<dyn EndpointResolver> = panic!("本探针只支持 Windows / macOS");
    let connector = ProductionConnector { resolver };

    let out = std::env::temp_dir().join(format!("tuvis-live-decision-{}.json", env.label()));
    match execute(&connector, &env, Some(out)).await {
        Ok(outcome) => {
            print_summary(&outcome);
            println!("\n===== 脱敏夹具 =====");
            println!(
                "{}",
                serde_json::to_string_pretty(&outcome.fixture).expect("序列化夹具")
            );
            // 无回执 → 如实报告（文件已写，夹具里 response 段是「无回执」）
            if outcome
                .receipt
                .as_ref()
                .is_some_and(|r| r.result_type == "none")
            {
                let note = outcome
                    .receipt
                    .as_ref()
                    .and_then(|r| r.note.clone())
                    .unwrap_or_else(|| "未知".to_string());
                let path = outcome
                    .out_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                panic!("决策已发出但**无回执**（{note}）——不重试、不猜。夹具已写：{path}");
            }
        }
        Err(ToolError::Gate(gate_error)) => {
            eprintln!("闸门未通过：{}", gate_error.message);
            if gate_error.kind == GateErrorKind::MissingThread {
                let candidates = candidate_threads(5);
                eprintln!(
                    "最近会话候选（选一个填入 {ENV_THREAD}，本探针**不自己挑**）：\n{}",
                    candidates.join("\n")
                );
            }
            panic!("闸门未通过：{}", gate_error.message);
        }
        Err(ToolError::Refused(message)) => {
            eprintln!("拒绝发帧：{message}");
            panic!("{message}");
        }
        Err(ToolError::Failed(message)) => {
            eprintln!("探针失败：{message}");
            panic!("{message}");
        }
    }
}

// ---------------------------------------------------------------- 自测（假体，不连真机）

/// 本地假 Desktop（duplex 一端）：帧级收发 + 收到帧的记录（自测断言用）。
struct FakePeer {
    io: DuplexStream,
    backlog: std::collections::VecDeque<serde_json::Value>,
    received: Vec<serde_json::Value>,
}

impl FakePeer {
    fn new() -> (Box<dyn IpcIo>, Self) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        (
            Box::new(client),
            Self {
                io: server,
                backlog: std::collections::VecDeque::new(),
                received: Vec::new(),
            },
        )
    }

    /// 读一帧（返回 JSON）。
    async fn recv(&mut self) -> serde_json::Value {
        if let Some(f) = self.backlog.pop_front() {
            return f;
        }
        let mut decoder = FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = self.io.read(&mut buf).await.expect("peer read");
            assert!(n > 0, "peer EOF");
            let mut frames = decoder.push(&buf[..n]).expect("decode");
            if frames.is_empty() {
                continue;
            }
            let first = frames.remove(0);
            for f in frames {
                self.backlog.push_back(f);
            }
            self.received.push(first.clone());
            return first;
        }
    }

    /// 读一帧（带超时；超时即失败并给出上下文）。
    async fn recv_expect(&mut self, what: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(10), self.recv())
            .await
            .unwrap_or_else(|_| panic!("{what} 帧未在 10s 内到达"))
    }

    /// 发一帧。
    async fn send(&mut self, value: &serde_json::Value) {
        let frame = encode_frame(value).expect("encode");
        self.io.write_all(&frame).await.expect("peer write");
    }
}

/// 假连接器：endpoint = 未登记 build（`26.1016.6573.0`）→ 走 unverified 档；
/// 记录是否被连接（自测断言「闸门不过 → 零帧」）。
struct FakeConnector {
    io: StdMutex<Option<Box<dyn IpcIo>>>,
    connected: AtomicBool,
}

impl FakeConnector {
    fn new(io: Box<dyn IpcIo>) -> Self {
        Self {
            io: StdMutex::new(Some(io)),
            connected: AtomicBool::new(false),
        }
    }
}

impl Connector for FakeConnector {
    fn connect_endpoint(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(BoxIo, DesktopEndpoint), ProtocolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.connected.store(true, Ordering::SeqCst);
            let io = self
                .io
                .lock()
                .expect("lock")
                .take()
                .ok_or_else(|| ProtocolError::Disconnected("fake: 已连接过一次".into()))?;
            Ok((
                io,
                DesktopEndpoint {
                    platform: "windows".into(),
                    path: "fake-endpoint".into(),
                    build: Some("26.1016.6573.0".into()),
                },
            ))
        })
    }
}

/// 自测用的会话 id（占位 UUID；不连真机）。
const FAKE_THREAD: &str = "00000000-0000-4000-8000-00000000f001";

/// 假 Desktop 脚本：握手 → discovery 回执 → following 声明 → 推送 pending snapshot
/// → （按需）回应决策帧 → 看到 `following:false` 收尾。
///
/// 返回：收到的**决策帧**列表 + 是否看到 `following:false`。
async fn drive_fake_desktop(
    peer: &mut FakePeer,
    pending: serde_json::Value,
    answer_decision: bool,
) -> (Vec<serde_json::Value>, bool) {
    // initialize
    let init = peer.recv_expect("initialize").await;
    assert_eq!(init["method"], "initialize");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": "owner-1", "result": { "clientId": "owner-1" }
    }))
    .await;
    // discovery
    let disc = peer.recv_expect("discovery").await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": disc["requestId"],
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": "owner-1", "result": {}
    }))
    .await;
    // 主循环：按 method 分派，直到看到 following:false
    let mut decision_frames: Vec<serde_json::Value> = Vec::new();
    let mut unfollow_seen = false;
    let mut snapshot_sent = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let frame = peer.recv_expect("工具帧").await;
        let method = frame["method"].as_str().unwrap_or("");
        match method {
            "thread-stream-following-changed" => {
                if frame["params"]["following"] == serde_json::json!(true) {
                    // 首次声明 following → 推一条带 pending 的 snapshot
                    if !snapshot_sent {
                        snapshot_sent = true;
                        peer.send(&serde_json::json!({
                            "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
                            "params": {
                                "conversationId": FAKE_THREAD, "hostId": "local",
                                "change": {
                                    "type": "snapshot", "revision": 7,
                                    "conversationState": { "turns": [], "requests": [pending] }
                                }
                            }
                        }))
                        .await;
                    }
                } else {
                    unfollow_seen = true;
                    break;
                }
            }
            "thread-stream-following-status-requested" => continue,
            METHOD_COMMAND_DECISION | METHOD_FILE_DECISION => {
                decision_frames.push(frame.clone());
                if answer_decision {
                    peer.send(&serde_json::json!({
                        "type": "response", "requestId": frame["requestId"],
                        "resultType": "success", "method": frame["method"],
                        "handledByClientId": "owner-1", "result": { "ok": true }
                    }))
                    .await;
                }
            }
            _ => continue,
        }
    }
    (decision_frames, unfollow_seen)
}

/// 命令审批的 pending（夹具形态；命令正文是**可识别的哨兵串**，自测要断言它没落盘）。
fn fake_command_pending() -> serde_json::Value {
    serde_json::json!({
        "id": 59,
        "method": "item/commandExecution/requestApproval",
        "params": {
            "kind": "command",
            "threadId": FAKE_THREAD,
            "turnId": "11111111-2222-4333-8444-555555555555",
            "itemId": "exec-666666666666",
            "startedAtMs": 1792408625171_u64,
            "environmentId": "local",
            "reason": "需要执行一条命令",
            "command": "rm -rf D:/secret-sentinel-dir",
            "cwd": "D:/secret-sentinel-dir",
            "availableDecisions": ["accept", "cancel"]
        }
    })
}

/// 计划实现审批的 pending（**不是**命令/文件审批 → 必须拒绝发帧）。
fn fake_plan_pending() -> serde_json::Value {
    serde_json::json!({
        "id": "plan-request-0000000000000000000000000000000000000000",
        "method": "item/plan/requestImplementation",
        "params": { "planContent": "计划正文（不应落盘）" }
    })
}

/// 自测 ①：缺 `TUVIS_LIVE_DECISION=1` → **不连接、不发任何帧**。
#[tokio::test]
async fn missing_authorization_sends_no_frames() {
    let (io, mut peer) = FakePeer::new();
    let connector = FakeConnector::new(io);
    let env = ToolEnv {
        authorized: None,
        thread: Some(FAKE_THREAD.to_string()),
        decision: Some(DECISION_ACCEPT.to_string()),
        ..ToolEnv::default()
    };
    let err = execute(&connector, &env, None)
        .await
        .expect_err("缺授权必须失败");
    match err {
        ToolError::Gate(gate_error) => {
            assert_eq!(gate_error.kind, GateErrorKind::MissingAuthorization);
        }
        other => panic!("闸门失败类型不对：{other:?}"),
    }
    assert!(
        !connector.connected.load(Ordering::SeqCst),
        "闸门不过时**不得连接**（零帧）"
    );
    let drain = tokio::time::timeout(Duration::from_millis(200), peer.recv()).await;
    assert!(drain.is_err(), "闸门不过时对端不得收到任何帧");
    assert!(peer.received.is_empty(), "对端收到的帧数必须为 0");
}

/// 自测 ①b：其它两个环境变量缺失/非法 → 同样零帧。
#[tokio::test]
async fn missing_thread_or_bad_decision_sends_no_frames() {
    // 缺 thread
    let (io, mut peer) = FakePeer::new();
    let connector = FakeConnector::new(io);
    let env = ToolEnv {
        authorized: Some("1".to_string()),
        thread: None,
        decision: Some(DECISION_ACCEPT.to_string()),
        ..ToolEnv::default()
    };
    match execute(&connector, &env, None)
        .await
        .expect_err("缺 thread 必须失败")
    {
        ToolError::Gate(gate_error) => {
            assert_eq!(gate_error.kind, GateErrorKind::MissingThread);
        }
        other => panic!("闸门失败类型不对：{other:?}"),
    }
    assert!(!connector.connected.load(Ordering::SeqCst));

    // 非法决策词
    let (io, _peer) = FakePeer::new();
    let connector = FakeConnector::new(io);
    let env = ToolEnv {
        authorized: Some("1".to_string()),
        thread: Some(FAKE_THREAD.to_string()),
        decision: Some("decline".to_string()),
        ..ToolEnv::default()
    };
    match execute(&connector, &env, None)
        .await
        .expect_err("非法决策词必须失败")
    {
        ToolError::Gate(gate_error) => {
            assert_eq!(gate_error.kind, GateErrorKind::InvalidDecision);
        }
        other => panic!("闸门失败类型不对：{other:?}"),
    }
    assert!(!connector.connected.load(Ordering::SeqCst));
    let drain = tokio::time::timeout(Duration::from_millis(200), peer.recv()).await;
    assert!(drain.is_err(), "闸门不过时对端不得收到任何帧");
}

/// 自测 ②：pending 是 `item/plan/requestImplementation` → **拒绝发帧**（零决策帧）。
#[tokio::test]
async fn plan_pending_is_refused_without_decision_frame() {
    let (io, peer) = FakePeer::new();
    let connector = FakeConnector::new(io);
    let env = ToolEnv {
        authorized: Some("1".to_string()),
        thread: Some(FAKE_THREAD.to_string()),
        decision: Some(DECISION_ACCEPT.to_string()),
        ..ToolEnv::default()
    };
    let driver = tokio::spawn(async move {
        let mut peer = peer;
        drive_fake_desktop(&mut peer, fake_plan_pending(), false).await
    });
    let err = execute(&connector, &env, None)
        .await
        .expect_err("plan pending 必须被拒绝");
    match err {
        ToolError::Refused(message) => {
            assert!(
                message.contains("item/plan/requestImplementation"),
                "拒绝原因必须写明该 method：{message}"
            );
        }
        other => panic!("失败类型不对：{other:?}"),
    }
    let (decision_frames, _) = driver.await.expect("driver 任务");
    assert!(
        decision_frames.is_empty(),
        "plan pending 绝不能发决策帧（实测发出 {} 帧）",
        decision_frames.len()
    );
}

/// 自测 ③：命令审批 + `cancel` → **只发一帧**，方法名/params 正确，随后 unfollow。
#[tokio::test]
async fn command_approval_sends_exactly_one_cancel_frame() {
    let (io, peer) = FakePeer::new();
    let connector = FakeConnector::new(io);
    let env = ToolEnv {
        authorized: Some("1".to_string()),
        thread: Some(FAKE_THREAD.to_string()),
        decision: Some(DECISION_CANCEL.to_string()),
        ..ToolEnv::default()
    };
    let driver = tokio::spawn(async move {
        let mut peer = peer;
        drive_fake_desktop(&mut peer, fake_command_pending(), true).await
    });
    let outcome = execute(&connector, &env, None)
        .await
        .expect("决策闭环应当成功");
    let (decision_frames, unfollow_seen) = driver.await.expect("driver 任务");

    // 只发一帧
    assert_eq!(
        decision_frames.len(),
        1,
        "决策帧只允许发一次（实测 {} 帧）",
        decision_frames.len()
    );
    let frame = &decision_frames[0];
    assert_eq!(frame["method"], METHOD_COMMAND_DECISION);
    assert_eq!(frame["type"], "request");
    // 版本从方法表取（command 决策方法 v1）
    assert_eq!(frame["version"], serde_json::json!(1));
    // params 形态：conversationId / requestId / decision
    assert_eq!(frame["params"]["conversationId"], FAKE_THREAD);
    assert_eq!(frame["params"]["requestId"], serde_json::json!(59));
    assert_eq!(frame["params"]["decision"], "cancel");
    assert!(
        frame["params"].get("permissions").is_none(),
        "command 变体绝不带 permissions"
    );
    // 目标 owner（信封级）
    assert_eq!(frame["targetClientId"], "owner-1");
    // 收尾：unfollow
    assert!(unfollow_seen, "决策后必须撤销 following（following:false）");
    // 回执形态
    let receipt = outcome.receipt.expect("应有回执");
    assert_eq!(receipt.result_type, "success");
    assert!(receipt.handled_by_present);
}

/// 自测 ④：脱敏输出**不含命令正文原文**（且保留结构性事实）。
#[tokio::test]
async fn sanitized_fixture_has_no_command_body() {
    let (io, peer) = FakePeer::new();
    let connector = FakeConnector::new(io);
    let env = ToolEnv {
        authorized: Some("1".to_string()),
        thread: Some(FAKE_THREAD.to_string()),
        decision: Some(DECISION_CANCEL.to_string()),
        ..ToolEnv::default()
    };
    let driver = tokio::spawn(async move {
        let mut peer = peer;
        drive_fake_desktop(&mut peer, fake_command_pending(), true).await
    });
    let outcome = execute(&connector, &env, None)
        .await
        .expect("决策闭环应当成功");
    let _ = driver.await.expect("driver 任务");

    let text = serde_json::to_string(&outcome.fixture).expect("序列化夹具");
    assert!(
        !text.contains("rm -rf D:/secret-sentinel-dir"),
        "夹具不得含命令正文原文：{text}"
    );
    assert!(
        !text.contains("D:/secret-sentinel-dir"),
        "夹具不得含真实路径：{text}"
    );
    assert!(
        !text.contains("11111111-2222-4333-8444-555555555555"),
        "夹具不得含真实 turnId：{text}"
    );
    assert!(
        !text.contains("exec-666666666666"),
        "夹具不得含真实 itemId：{text}"
    );
    assert!(
        !text.contains("1792408625171"),
        "夹具不得含真实 startedAtMs：{text}"
    );
    assert!(
        !text.contains(FAKE_THREAD),
        "夹具不得含真实 threadId：{text}"
    );
    // 结构性事实仍在
    assert!(
        text.contains("<synthetic command>"),
        "命令正文用占位：{text}"
    );
    assert!(
        text.contains("item/commandExecution/requestApproval"),
        "pending method 原值保留：{text}"
    );
    assert!(
        text.contains("\"decision\":\"cancel\""),
        "决策词保留：{text}"
    );
    assert!(
        text.contains("\"availableDecisions\":[\"accept\",\"cancel\"]"),
        "availableDecisions 的字符串词保留：{text}"
    );
    assert!(
        text.contains("\"resultType\":\"success\""),
        "回执 resultType 保留：{text}"
    );
    assert!(text.contains("\"ok\":true"), "回执 result 保留：{text}");
    // pending id 的 JSON 类型保留（整数原值）
    assert!(
        outcome.fixture["pendingRequest"]["id"] == serde_json::json!(59),
        "pending id 的整数形态保留"
    );
}

/// 自测 ⑤：文件审批走**自己的**决策方法（不与 command 混用）。
#[tokio::test]
async fn file_approval_uses_file_decision_method() {
    let (io, peer) = FakePeer::new();
    let connector = FakeConnector::new(io);
    let env = ToolEnv {
        authorized: Some("1".to_string()),
        thread: Some(FAKE_THREAD.to_string()),
        decision: Some(DECISION_ACCEPT.to_string()),
        ..ToolEnv::default()
    };
    let driver = tokio::spawn(async move {
        let mut peer = peer;
        drive_fake_desktop(
            &mut peer,
            serde_json::json!({
                "id": 21,
                "method": "item/fileChange/requestApproval",
                "params": { "cwd": "D:/secret-sentinel-dir", "availableDecisions": ["accept", "cancel"] }
            }),
            true,
        )
        .await
    });
    let _ = execute(&connector, &env, None)
        .await
        .expect("决策闭环应当成功");
    let (decision_frames, _) = driver.await.expect("driver 任务");
    assert_eq!(decision_frames.len(), 1);
    assert_eq!(
        decision_frames[0]["method"], METHOD_FILE_DECISION,
        "文件审批必须用自己的决策方法"
    );
    assert_eq!(decision_frames[0]["version"], serde_json::json!(1));
    assert_eq!(decision_frames[0]["params"]["decision"], "accept");
}

/// 自测 ⑥：可用性门——决策词不在 pending 的 availableDecisions 里 → 拒绝发帧。
#[tokio::test]
async fn decision_word_outside_offered_words_is_refused() {
    let (io, peer) = FakePeer::new();
    let connector = FakeConnector::new(io);
    let env = ToolEnv {
        authorized: Some("1".to_string()),
        thread: Some(FAKE_THREAD.to_string()),
        decision: Some(DECISION_CANCEL.to_string()),
        ..ToolEnv::default()
    };
    let driver = tokio::spawn(async move {
        let mut peer = peer;
        drive_fake_desktop(
            &mut peer,
            serde_json::json!({
                "id": 7,
                "method": "item/commandExecution/requestApproval",
                "params": { "command": "echo hi", "availableDecisions": ["accept"] }
            }),
            false,
        )
        .await
    });
    let err = execute(&connector, &env, None)
        .await
        .expect_err("词不匹配必须被拒绝");
    match err {
        ToolError::Refused(message) => {
            assert!(
                message.contains("availableDecisions"),
                "拒绝原因应写明服务端给的词：{message}"
            );
        }
        other => panic!("失败类型不对：{other:?}"),
    }
    let (decision_frames, _) = driver.await.expect("driver 任务");
    assert!(decision_frames.is_empty(), "词不匹配时不得发决策帧");
}

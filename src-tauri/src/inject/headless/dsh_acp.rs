//! P1 dsh 轨（H13）：`@deepseek-ai/dsh-acp` stdio JSON-RPC 客户端——Task 9。
//!
//! # 定位（设计 §6 + 计划「⚠️ Task 0 探测回填」T0.3/T0.4 定案，冲突以回填为准）
//!
//! 本模块是 dsh APP 会话**写侧动作**（发消息 / 中止 / 权限透传）的客户端本体；
//! 路由接线（`HeadlessKind::DshAcp` + api.rs ⑤b 臂）归 Task 10，本任务不触
//! api.rs/routing.rs。读链路零改动（H1 已交付）。
//!
//! # spawn 钉值（T0.3 候选 A 定案）
//!
//! `<dsh runtime node.exe> <asar解包目录>\dsh\node_modules\@deepseek-ai\dsh\lib\bin.js
//! --profile acp`；env = `DSH_HOME` 透传（真实 `~/.dsh`，与桌面共享凭据）+
//! `DSH_TELEMETRY_DISABLED=1`（探测/产品卫生）；cwd 任意。node **固定取 dsh 自带
//! runtime**（`<DSH_HOME>/dsh-runtimes/dsh-primary-runtime/dependencies/node/bin/`）
//! ——不用系统 node（探测报告 §1.1）；直连 `.exe`，无需 cmd 垫片。
//!
//! **asar 解包策略（实现者定案，登记于此）**：裸 node 不认 asar 路径 ⇒ MAM 侧须先把
//! desktop 安装的 `\dsh` 安装树解包到**受管缓存目录**。取「运行时首启幂等解包」：
//! - 缓存布局 = `<cache>/<desktopVersion>/dsh/...`（缓存键 = desktopVersion，探测态
//!   0.2.0-rc.2；version_gate 分诊对齐 desktopVersion——Task 10 接线时消费）；
//! - 幂等判据 = digest 标记文件（asar 的 size+mtime 指纹）逐字相等 + bin.js 在场 → 跳过；
//! - 解包器 = **Rust 侧最小 asar 读取器**（头长 + pickle JSON 头 + 连续 blob，按
//!   offset/size 拷文件），**不依赖 `@electron/asar`**（那条路要 node + asar 包，依赖更重）；
//!   只解 `\dsh` 子树（探测整包 383MB，子树远小于整包）；unpacked 条目从
//!   `<asar>.unpacked/` 伴生目录拷贝；integrity 哈希跳过（缓存幂等由 MAM 自家 digest
//!   标记承担）。生产实现走 `cfg(not(test))` 缝，真机行为 M25 验证。
//! - **陷阱红线（探测 §1.5 实事故）**：`--patch` 对 acp config 是**整对象替换**——只写
//!   单键会把 provider/model 抹掉。本客户端**不发 `--patch`、不覆写 acp config**；
//!   将来若要改 provider/model/页大小，必须整份写全，且另行立项取证。
//!
//! **DSH_HOME 首启副作用（预期内，登记）**：acp profile 为 shipped 模板，CLI 首启自动
//! 在真实 home 落 `profiles/acp/`（4 文件）；不碰 desktop/headless/web 三 profile
//! （探测 §1.5 卫生核查实证）。MAM 只读不写该目录。
//!
//! # 协议事实（ACP v1，stdio JSONL；stdout 只承载协议流量，stderr 零输出）
//!
//! - **initialize** → 响应含 `agentCapabilities.promptCapabilities.image`——**动态位**
//!   （随 provider/model 变），不缓存消费；**authenticate 恒成功免鉴权**（源码直读）。
//! - **三态靶向**（T0.4 §2.2/§2.3）：`session/list`（keyset-cursor 分页 + cwd 绝对路径
//!   筛选；条目形状 `{sessionId, cwd}` **无 provider 字段**）→ ①命中且非活跃 →
//!   `session/resume`（cwd 必填且须与持久化 cwd 一致）+ `session/prompt` 精确靶向；
//!   ②命中但桌面打开中 → resume 被单写者锁拒 → **如实拒**（不做 queue 臆测——README
//!   未提 queue）；③未命中/无目标 → `session/new`（**`mcpServers` 必填**，恒发 `[]`）。
//! - **单写者锁判据（本地定义，勿跨通道复用）**：`-32603` 与特征串
//!   `already owned by an active write handle` **同现**才定性为桌面写者占用——同码还有
//!   余额不足（`Insufficient Balance`）与 provider 不可用两类，特征串缺席一律按协议
//!   错误透传；provider 不可用（`no adapter registered for provider`，如 ollama 历史
//!   会话）单列变体。**锁在持久层写句柄，list 看不出谁活跃——resume 失败即占用证据**。
//! - **sessionId 形态二象性**：list/resume 用 `session-<uuid>`、new 返回裸 `<uuid>`——
//!   **按原样回传，绝不归一化**（探测 §2.4 实证）。
//! - **provider 预筛只能事后做**：list 条目无 provider 字段，事前预筛不可能；provider
//!   不可用只能经 resume 错误分类事后判（本客户端是精确 id 靶向，事后判 = 如实 Err；
//!   多候选轮换属 Task 10 接线层编排）。
//! - **prompt 完成判据** = 响应 `{stopReason}`——**按在场字符串透传，不硬编码枚举**
//!   （值域未验，M25 兜底）；成功流 update 六变体为**源码形状**（余额阻断未活体取证）：
//!   `agent_thought_chunk` / `agent_message_chunk`（`{messageId, content}`）/ `tool_call` /
//!   `tool_call_update` / `usage_update {used,size}`（上下文占用，**不是输出 token**——
//!   Receipt.tokens 恒 None 不冒充）/ `config_option_update`；嵌套层级未取证，双路防御
//!   解析（`params.update` 标准嵌套优先、params 平铺兜底）。
//! - **cancel = notification 形态**（无响应帧可核对）——发出后结算以在飞 prompt 响应的
//!   stopReason 为准；在飞取消效果未验（M25），如实注记不谎报。
//! - **request_permission**（服务端→客户端请求，`{sessionId, toolCall:{toolCallId},
//!   options:[allow-once/reject-once]}`）：**首批只收集展示不代答**（代答按钮列 P2——
//!   届时按 optionId 回 `outcome:{outcome:"selected",optionId}`）。
//! - **会话收尾纪律**：`session/close` 每会话必收尾（释放写锁，探测 §2.4）；持写者期间
//!   **优雅退出（关 stdin 等自然退出），绝不硬杀**（kill 仅宽限尽后兜底）。
//! - **`session/set_config_option`**：`value` 必须是 **JSON 字符串形态**（传数组被 schema
//!   拒）——类型缝保留、**不接线**（send/dispatch 不调它）。
//! - **范围边界（不做清单，设计 §6.1）**：mode（dsh-acp 刻意无）/ session/load·fork·delete
//!   （刻意无）/ SSE / 19387 web HTTP / webhook——客户端不实现，路由层如实 unsupported。
//!
//! # 调用契约（照 codex_app.rs 先例）
//!
//! [`DshAcpClient::send`] / [`DshAcpClient::session`] 是**同步阻塞**调用：prompt 等待窗
//! 最长 = 回合时长（模型侧结算，探测未验上界），单帧读预算 [`PROC_READ_TIMEOUT_MS`]
//! （300s）。调用方（Task 10 的 api.rs 端点）**必须放 `spawn_blocking`**——绝不在
//! async 运行时直呼，否则一个 dsh 回合就能把执行线程占满一个读预算周期。
//! 会话收尾的三条出路（close→exit / 超时跳 close 直接 exit / 拒绝即 exit）见
//! [`DshAcpClient::send`] 编排注释。

//! # 测试纪律（宪法级）
//!
//! 单测**绝不** spawn 真 node / 不碰真实 `~/.dsh`：进程出口全部经 [`SpawnSeam`] 缝注入
//! （tests 的 [`MockIo`] 桩，照 codex_app.rs 先例），生产缝 [`production_spawn_seam`]
//! 为 `cfg(not(test))`——测试构建零真实进程零网络；其验证面 = M25 真机验收。

use std::sync::Arc;

// 底座复用（turn.rs:325/336 单点口径）：Ok 回执的末条 assistant 摘要截断——
// 不在本模块内联第二份截断写法
use super::receipt::{Receipt, ReceiptStatus, Stage};
use super::turn::ok_receipt_with_assistant;

/// dsh CLI 的 acp profile 名（`--profile acp`；T0.3 候选 A 定案）
pub const ACP_PROFILE: &str = "acp";
/// harness home 重定向钮（透传真实 `~/.dsh`，与桌面共享凭据——探测 §1.5）
pub const DSH_HOME_ENV: &str = "DSH_HOME";
/// 遥测开关（任何非空值即关——产品卫生，探测定案）
pub const DSH_TELEMETRY_DISABLED_ENV: &str = "DSH_TELEMETRY_DISABLED";
/// 遥测关闭值
pub const TELEMETRY_OFF: &str = "1";
/// ACP 协议版本（探测：initialize params `{"protocolVersion":1,…}`）
pub const PROTOCOL_VERSION: i64 = 1;

/// JSON-RPC method not found（方法缺席 → unavailable；版本漂移的如实信号）
pub const METHOD_NOT_FOUND_CODE: i64 = -32601;
/// JSON-RPC internal error（dsh 的写者锁/回合失败/provider 面共用此码——
/// **必须特征串同现才定性**，见 [`DESKTOP_WRITER_LOCK_MARKER`]）
pub const INTERNAL_ERROR_CODE: i64 = -32603;

/// dsh 侧单写者锁特征串（T0.4 §2.3 活体）。**本地定义**：与 codex 侧
/// `codex.rs::WRITER_LOCK_MARKER`（"already has an active writer"）是**不同串**——
/// 三家写者语义同源但特征文本各异，跨通道勿复用（复用会互相误判）。
pub const DESKTOP_WRITER_LOCK_MARKER: &str = "already owned by an active write handle";
/// provider 不可用特征串（T0.4 §2.3：历史 ollama 会话 resume 必败——acp profile 不带
/// 该 provider 适配器；list 条目无 provider 字段，只能事后经此分类判）
pub const PROVIDER_UNAVAILABLE_MARKER: &str = "no adapter registered for provider";

/// 活跃会话（桌面打开中）如实拒文案（设计 §6.1 红线：不做 queue 臆测）
pub const ACTIVE_SESSION_REFUSAL: &str = "会话正被桌面使用，请关闭后再投";
/// 审计/回执分类词：桌面写者占用（T0.1 同族旗标词的 dsh 版）
pub const DESKTOP_WRITER_ACTIVE: &str = "desktop_writer_active";
/// 审计/回执分类词：provider 不可用（事后判——list 无 provider 字段）
pub const PROVIDER_UNAVAILABLE: &str = "provider_unavailable";
/// cancel 的如实注记（notification 形态无回执可核对——结算以在飞回合响应为准）
pub const CANCEL_NOTE: &str = "已发出 session/cancel 通知（notification 形态，无响应帧可核对）——\
     结算以在飞回合响应的 stopReason 为准（取消效果待 M25 真机验证）";
/// 范围边界（设计 §6.1 不做清单）：客户端不实现这些方法，路由层如实 unsupported
pub const UNSUPPORTED_SURFACE: &str = "mode/session-load/fork/delete/sse/web-http-19387/webhook";

/// list 分页护栏：keyset cursor 由**服务端**页大小驱动（无 limit 参数——T0.4 §2.2），
/// 客户端侧防失控上限（护栏触发按通道错误面处理，不静默截断）
pub const MAX_LIST_PAGES: usize = 200;

/// 受管缓存内的解包子树相对布局（单点：`dsh/node_modules/@deepseek-ai/dsh/lib/bin.js`）
pub const EXTRACT_SUBTREE_REL: &str = "dsh/node_modules/@deepseek-ai/dsh/lib/bin.js";
/// 解包幂等的 digest 标记文件名（内容 = asar 的 size+mtime 指纹）
pub const EXTRACT_MARKER_FILE: &str = ".mam-asar-digest";

// ============================================================
// 帧解析（宽松方言：判序照 codex_app.rs 先例——id/result|error/method 组合，不按 2.0 严格校验）
// ============================================================

/// 单帧分类（stdio JSONL 每行）。dsh-acp 为标准 ACP（帧带 `jsonrpc` 字段），但解析
/// **不依赖它**（在场不拒、缺席不怪）——与 codex_app 同款宽松判序，防方言漂移误杀。
#[derive(Debug, Clone, PartialEq)]
pub enum WireFrame {
    /// 响应帧（id + `result`|`error`）——请求-响应匹配的唯一来源
    Response { id: u64, payload: FramePayload },
    /// 服务端→客户端请求（id + method：`session/request_permission` 等）——
    /// **只收集不代答**（代答列 P2）
    ServerRequest {
        id: u64,
        method: String,
        params: serde_json::Value,
    },
    /// 通知旁流（无 id：`session/update` 流等）
    Notification {
        method: String,
        params: serde_json::Value,
    },
    /// 未知行（非 JSON / 无已知键组合）——只计数不解释
    Unknown,
}

/// 响应帧载荷
#[derive(Debug, Clone, PartialEq)]
pub enum FramePayload {
    Ok(serde_json::Value),
    Err(FrameError),
}

/// 结构化协议错误（code+message 双全才认，残缺归未知——不猜）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    pub code: i64,
    pub message: String,
}

/// 宽松方言单帧解析（纯核，判序照 codex_app.rs:110 先例）：① 有 `id`（u64）且有
/// `result` → 响应 Ok；② 有 `id` 且有结构化 `error` → 响应 Err；③ 有 `id` 且有
/// `method` → 服务端请求（params 透传——request_permission 展示面要用）；④ 无 `id`
/// 有 `method` → 通知；⑤ 其余 → 未知行。`jsonrpc` 全程不看。
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
        // 结构化错误双键齐全才认（code 缺场/非数的错误形状归未知——不猜）
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
            // 服务端请求的 params 透传（request_permission 展示面要用）
            let params = obj
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return WireFrame::ServerRequest {
                id,
                method: m.to_string(),
                params,
            };
        }
        return WireFrame::Unknown; // 有 id 但无 result/error/method——不猜
    }
    if let Some(m) = obj.get("method").and_then(serde_json::Value::as_str) {
        let params = obj
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        return WireFrame::Notification {
            method: m.to_string(),
            params,
        };
    }
    WireFrame::Unknown
}

// ============================================================
// 错误分类（调用方据此降级/换候选）
// ============================================================

/// 客户端动作错误。`wire_word()` 供审计/回执分类列（Task 10 消费）；[`Display`] 面向用户。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DshAcpError {
    /// 通道级失败：spawn 失败 / EOF 无响应 / initialize·authenticate 失败 / 响应形状不可解析
    Channel(String),
    /// 目标不可定位/非法（空文本 / cwd 非绝对路径）——投递前拒绝，零协议副作用
    InvalidTarget(String),
    /// 活跃会话（桌面打开中）：resume 被 `-32603` + [`DESKTOP_WRITER_LOCK_MARKER`]
    /// 同现拒（**定性要求特征串同现**——同码还有余额不足与 provider 不可用，勿混判）
    DesktopWriterActive { lock_message: String },
    /// provider 不可用（如 ollama 历史会话——acp profile 不带该适配器，resume 必败）。
    /// 事后判：list 条目无 provider 字段，事前预筛不可能（Task 10 可据此换候选）
    ProviderUnavailable { message: String },
    /// 方法缺席（-32601 形态）→ unavailable（版本漂移的如实信号；非 panic 非成功）
    MethodUnavailable { method: String, message: String },
    /// 其余协议错误（code/message 原样透传——含 `Insufficient Balance` 一类回合失败）
    Rpc { code: i64, message: String },
}

impl DshAcpError {
    /// 审计/回执分类词（单点）
    pub const fn wire_word(&self) -> &'static str {
        match self {
            DshAcpError::Channel(_) => "channel_error",
            DshAcpError::InvalidTarget(_) => "invalid_target",
            DshAcpError::DesktopWriterActive { .. } => DESKTOP_WRITER_ACTIVE,
            DshAcpError::ProviderUnavailable { .. } => PROVIDER_UNAVAILABLE,
            DshAcpError::MethodUnavailable { .. } => "method_unavailable",
            DshAcpError::Rpc { .. } => "rpc_error",
        }
    }
}

impl std::fmt::Display for DshAcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DshAcpError::Channel(m) => write!(f, "dsh 协议通道不可用：{m}"),
            DshAcpError::InvalidTarget(m) => write!(f, "目标不可用：{m}"),
            DshAcpError::DesktopWriterActive { lock_message } => {
                write!(f, "{ACTIVE_SESSION_REFUSAL}（桌面写者锁：{lock_message}）")
            }
            DshAcpError::ProviderUnavailable { message } => {
                write!(f, "会话的 provider 不可用（如 ollama 历史会话）：{message}")
            }
            DshAcpError::MethodUnavailable { method, message } => {
                write!(f, "协议方法不可用（{method}）：{message}")
            }
            DshAcpError::Rpc { code, message } => write!(f, "协议错误 {code}：{message}"),
        }
    }
}

/// 错误帧分类（纯核）：-32601 → 方法缺席；-32603 + 写者锁特征串**同现** → 桌面写者占用；
/// provider 特征串 → provider 不可用；其余原样透传（同码的 `Insufficient Balance`
/// 一类不带特征串 → Rpc——**裸 -32603 不当锁**）
fn classify(method: &str, e: FrameError) -> DshAcpError {
    if e.code == METHOD_NOT_FOUND_CODE {
        return DshAcpError::MethodUnavailable {
            method: method.to_string(),
            message: e.message,
        };
    }
    // -32603 与锁特征串**同现**才定性桌面写者（同码还有余额不足等——裸码不当锁）
    if e.code == INTERNAL_ERROR_CODE && e.message.contains(DESKTOP_WRITER_LOCK_MARKER) {
        return DshAcpError::DesktopWriterActive {
            lock_message: e.message,
        };
    }
    // provider 特征串（探测形态为 -32603，按特征串判更稳——码可能随版本漂移）
    if e.message.contains(PROVIDER_UNAVAILABLE_MARKER) {
        return DshAcpError::ProviderUnavailable { message: e.message };
    }
    DshAcpError::Rpc {
        code: e.code,
        message: e.message,
    }
}

// ============================================================
// spawn 规格 + 执行缝（唯一进程出口）
// ============================================================

/// dsh-acp spawn 规格（argv + 显式 env；生产另继承父进程 env）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub argv: Vec<String>,
    /// MAM 显式注入的 env（`DSH_HOME` 透传 + 关遥测）；生产实现叠加在父 env 之上
    pub env: Vec<(String, String)>,
}

/// T0.3 钉值 argv：`<node.exe> <bin.js> --profile acp`。**不含 `--patch`**——
/// acp config 覆盖是整对象替换（探测 §1.5 实事故），本客户端不覆写 config
pub fn acp_argv(node_exe: &str, bin_js: &str) -> Vec<String> {
    vec![
        node_exe.to_string(),
        bin_js.to_string(),
        "--profile".to_string(),
        ACP_PROFILE.to_string(),
    ]
}

/// env 构造：`DSH_HOME` 透传 + `DSH_TELEMETRY_DISABLED=1`
pub fn acp_env(dsh_home: &str) -> Vec<(String, String)> {
    vec![
        (DSH_HOME_ENV.to_string(), dsh_home.to_string()),
        (
            DSH_TELEMETRY_DISABLED_ENV.to_string(),
            TELEMETRY_OFF.to_string(),
        ),
    ]
}

/// desktopVersion → 缓存子目录名（防路径注入：白名单字符外的全替 `_`；空串兜底 `unknown`）
pub fn sanitize_version(version: &str) -> String {
    let cleaned: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // spec m1：`.`/`..` 不得作缓存子目录名（越界写根目录 / 路径穿越）——回落 unknown
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// 受管缓存内的 bin.js 路径（`<cache>/<version>/dsh/node_modules/…/bin.js`；
/// 布局单点 = [`EXTRACT_SUBTREE_REL`]）
pub fn bin_js_path(cache_root: &str, desktop_version: &str) -> String {
    format!(
        "{cache_root}/{}/{EXTRACT_SUBTREE_REL}",
        sanitize_version(desktop_version)
    )
}

/// 受管缓存内的 digest 标记文件路径
pub fn extract_marker_path(cache_root: &str, desktop_version: &str) -> String {
    format!(
        "{cache_root}/{}/{EXTRACT_MARKER_FILE}",
        sanitize_version(desktop_version)
    )
}

/// asar 指纹（size+mtime——幂等判据的原料；不引入哈希依赖，383MB 级文件 sha 太贵）
pub fn extract_fingerprint(size: u64, mtime_secs: u64) -> String {
    format!("size={size},mtime={mtime_secs}")
}

/// 解包决策（幂等）：标记在场且逐字等于当前指纹 → 跳过；否则解包
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractDecision {
    Skip,
    Extract,
}

/// 幂等解包决策（纯核）：首启（无标记）/指纹漂移（asar 升级）→ Extract；
/// 标记逐字相等 → Skip
pub fn extract_decision(marker: Option<&str>, fingerprint: &str) -> ExtractDecision {
    match marker {
        Some(m) if m == fingerprint => ExtractDecision::Skip,
        _ => ExtractDecision::Extract,
    }
}

/// 一枚 dsh-acp 连接的传输面（**执行缝**）。生产 = stdio 子进程
/// （[`production_spawn_seam`]，`cfg(not(test))`）；测试 = 脚本桩（tests 的 [`MockIo`]）。
pub trait AcpIo: Send {
    /// 写一行（JSON 帧原文；实现自行补换行与 flush）
    fn send(&mut self, line: &str) -> Result<(), String>;
    /// 读下一行 stdout：`None` = EOF；`Some(Err)` = IO 错误/超时
    fn recv(&mut self) -> Option<Result<String, String>>;
    /// 优雅收尾：**关 stdin 让进程自然退出**（持写者期间绝不硬杀；kill 只能是
    /// 宽限后的最后兜底——探测 §1.2：stdin 关闭后 2s 内 exit 0）
    fn finish(&mut self);
}

/// spawn 缝（唯一进程出口；测试注入脚本桩，生产 [`production_spawn_seam`]）
pub type SpawnSeam = Arc<dyn Fn(&SpawnSpec) -> Result<Box<dyn AcpIo>, String> + Send + Sync>;

// ============================================================
// session/list 条目 + prompt 通知流产物（纯核形状）
// ============================================================

/// `session/list` 条目（探测 §2.2：形状 `{sessionId, cwd}`——**无时间戳、无 provider**；
/// 两键缺一跳过该条目，不猜）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub session_id: String,
    pub cwd: String,
}

/// `session/update` 六变体（T0.4 §2.5 **源码形状**——余额阻断未活体取证，M25 转正；
/// 字段全防御抽取，缺场 = None 不猜）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateVariant {
    ThoughtChunk {
        message_id: Option<String>,
        text: Option<String>,
    },
    MessageChunk {
        message_id: Option<String>,
        text: Option<String>,
    },
    ToolCall {
        tool_call_id: Option<String>,
        title: Option<String>,
        status: Option<String>,
    },
    ToolCallUpdate {
        tool_call_id: Option<String>,
        status: Option<String>,
    },
    /// 上下文用量（`{used, size}`——**是上下文窗口占用不是输出 token**，
    /// Receipt.tokens 恒 None 不冒充）
    Usage {
        used: Option<u64>,
        size: Option<u64>,
    },
    ConfigOption,
    Unknown,
}

/// 从 `session/update` 通知 params 解析变体（纯核）。嵌套层级未活体取证——**双路防御**：
/// `params.update`（ACP v1 标准嵌套）优先、params 平铺兜底；两路都无已知变体键 → Unknown
/// 六变体键名（T0.4 §2.5 源码形状——snake_case 命名，非 ACP 标准 camelCase）
const VARIANT_KEYS: [&str; 6] = [
    "agent_thought_chunk",
    "agent_message_chunk",
    "tool_call",
    "tool_call_update",
    "usage_update",
    "config_option_update",
];

fn has_known_variant_key(body: &serde_json::Value) -> bool {
    VARIANT_KEYS.iter().any(|k| body.get(k).is_some())
}

/// 嵌套层级未活体取证（余额阻断）——**双路防御**：`params.update`（ACP v1 标准嵌套）
/// 优先、params 平铺兜底；两路都无已知变体键 → None（不猜）
fn update_body(params: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(nested) = params.get("update").filter(|v| has_known_variant_key(v)) {
        return Some(nested);
    }
    if has_known_variant_key(params) {
        return Some(params);
    }
    None
}

pub fn parse_update(params: &serde_json::Value) -> UpdateVariant {
    use serde_json::Value;
    let Some(body) = update_body(params) else {
        return UpdateVariant::Unknown;
    };
    let s_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let text_of = |v: &Value| {
        v.get("content")
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    if let Some(v) = body.get("agent_thought_chunk") {
        return UpdateVariant::ThoughtChunk {
            message_id: s_of(v, "messageId"),
            text: text_of(v),
        };
    }
    if let Some(v) = body.get("agent_message_chunk") {
        return UpdateVariant::MessageChunk {
            message_id: s_of(v, "messageId"),
            text: text_of(v),
        };
    }
    if let Some(v) = body.get("tool_call") {
        return UpdateVariant::ToolCall {
            tool_call_id: s_of(v, "toolCallId"),
            title: s_of(v, "title"),
            status: s_of(v, "status"),
        };
    }
    if let Some(v) = body.get("tool_call_update") {
        return UpdateVariant::ToolCallUpdate {
            tool_call_id: s_of(v, "toolCallId"),
            status: s_of(v, "status"),
        };
    }
    if let Some(v) = body.get("usage_update") {
        return UpdateVariant::Usage {
            used: v.get("used").and_then(Value::as_u64),
            size: v.get("size").and_then(Value::as_u64),
        };
    }
    if body.get("config_option_update").is_some() {
        return UpdateVariant::ConfigOption;
    }
    UpdateVariant::Unknown
}

/// update 流累积摘要（prompt 等待窗内逐帧累积；未知变体只计数不解释）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateStreamSummary {
    pub thought_chunks: usize,
    pub message_chunks: usize,
    pub tool_calls: usize,
    pub tool_call_updates: usize,
    pub usage_updates: usize,
    pub config_option_updates: usize,
    pub unknown_updates: usize,
    /// 上下文用量（取最新一帧 usage_update——是窗口占用不是输出 token，如实透出供审计）
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
    /// 末条 agent 消息全文（按 messageId 聚合 chunk 追加、按首见顺序取末条；空流 = None）
    pub last_message: Option<String>,
    /// 聚合缓冲（messageId → 拼接文本，首见顺序）——私有：外部只读 [`Self::last_message`]
    msg_order: Vec<(String, String)>,
}

impl UpdateStreamSummary {
    /// 累积一帧变体
    pub fn push(&mut self, v: UpdateVariant) {
        match v {
            UpdateVariant::ThoughtChunk { .. } => self.thought_chunks += 1,
            UpdateVariant::MessageChunk { message_id, text } => {
                self.message_chunks += 1;
                // 同 messageId 的流式 chunk 追加；新 messageId 开新缓冲（首见顺序）。
                // Minor 5：末条消息的展示缓冲**增量追加**——逐 chunk 全量 clone 是
                // O(chunk 数 × 消息长) 的平方级；非末条消息的迟到 chunk 只进聚合缓冲
                //（等它成为末条时由首块整体带入，不破坏「末条 = 首见顺序最后者」语义）
                let id = message_id.unwrap_or_default();
                let text = text.unwrap_or_default();
                match self.msg_order.iter().position(|(mid, _)| *mid == id) {
                    Some(i) => {
                        self.msg_order[i].1.push_str(&text);
                        if i + 1 == self.msg_order.len() {
                            match self.last_message.as_mut() {
                                Some(m) => m.push_str(&text),
                                None => {
                                    self.last_message = Some(self.msg_order[i].1.clone());
                                }
                            }
                        }
                    }
                    None => {
                        self.msg_order.push((id, text.clone()));
                        self.last_message = Some(text);
                    }
                }
            }
            UpdateVariant::ToolCall { .. } => self.tool_calls += 1,
            UpdateVariant::ToolCallUpdate { .. } => self.tool_call_updates += 1,
            UpdateVariant::Usage { used, size } => {
                self.usage_updates += 1;
                if used.is_some() {
                    self.context_used = used;
                }
                if size.is_some() {
                    self.context_size = size;
                }
            }
            UpdateVariant::ConfigOption => self.config_option_updates += 1,
            UpdateVariant::Unknown => self.unknown_updates += 1,
        }
    }
}

/// prompt 回执（完成判据 = 响应 `{stopReason}`——**在场透传不硬编码枚举**，值域未验 M25 兜底）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptOutcome {
    pub stop_reason: Option<String>,
    pub updates: UpdateStreamSummary,
    /// 回合墙钟（prompt 起止；不含 list/resume 编排耗时——口径登记）
    pub duration_ms: u64,
}

/// `session/request_permission` 透传件（只收集展示，**永不回发 outcome 帧**——代答列 P2）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

/// 服务端权限请求（帧形状 = `{sessionId, toolCall:{toolCallId}, options:[…]}`——T0.4 §2.7）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// 服务端请求 id（**只登记不回发**——回发 outcome 即代答，本批禁止）
    pub id: u64,
    pub session_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub options: Vec<PermissionOption>,
}

impl PermissionRequest {
    /// 从 ServerRequest 帧防御抽取（缺场 = None/空，不猜）
    fn from_frame(id: u64, params: &serde_json::Value) -> Self {
        use serde_json::Value;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let tool_call_id = params
            .get("toolCall")
            .and_then(|t| t.get("toolCallId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        Some(PermissionOption {
                            option_id: o.get("optionId")?.as_str()?.to_string(),
                            name: o
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            kind: o
                                .get("kind")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            id,
            session_id,
            tool_call_id,
            options,
        }
    }

    /// 上屏文案（设计 §6.1：「dsh 请求权限：〈内容〉」——Task 10 接线到问答/审批载荷族）
    pub fn display(&self) -> String {
        let opts = if self.options.is_empty() {
            "（无选项）".to_string()
        } else {
            self.options
                .iter()
                .map(|o| {
                    if o.name.is_empty() {
                        o.option_id.clone()
                    } else {
                        o.name.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" / ")
        };
        format!(
            "DSH_ACP_PERMISSION_PROMPT: dsh 请求权限：toolCallId={}，选项：{opts}",
            self.tool_call_id
                .as_deref()
                .unwrap_or("DSH_ACP_UNKNOWN_ID: （未知）")
        )
    }
}

/// cancel 回执（notification 形态——无响应帧可核对，`sent` 只代表已写出，**不代表已确认**）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelNotice {
    pub session_id: String,
    pub sent: bool,
    /// 如实注记（[`CANCEL_NOTE`] 钉值——取消效果待 M25 真机验证）
    pub note: String,
}

// ============================================================
// 会话（spawn + initialize + authenticate → 调用 → close → 退出）
// ============================================================

/// 一枚 dsh-acp 连接（spawn + 握手后的活通道）。
///
/// 生命周期归调用方（per-action 短命：起 → 调用 → `session/close` 释放写锁 → 优雅退出；
/// 错误路径由 Drop 兜底优雅收尾）。旁流分道收集：update 流进 prompt 等待窗的累积器、
/// 其余通知进 `notifications`、服务端请求进 `permission_requests`/`server_requests_other`。
pub struct DshAcpSession {
    io: Box<dyn AcpIo>,
    next_id: u64,
    /// 通知旁流（无 id 帧 method 序列——不参与响应匹配）
    pub notifications: Vec<String>,
    /// `session/request_permission` 收集（只展示不代答——P2 代答的扩展位在此）
    pub permission_requests: Vec<PermissionRequest>,
    /// 其余服务端请求（id+method 旁记——不猜不代答）
    pub server_requests_other: Vec<(u64, String)>,
    pub unknown_frames: usize,
    pub stray_responses: usize,
    /// `agentInfo.name`（版本坐标登记不硬编码）
    pub agent_name: Option<String>,
    /// `promptCapabilities.image`——**动态位**（随 provider/model 变），登记不缓存消费
    pub prompt_image_capability: Option<bool>,
}

impl DshAcpSession {
    /// spawn + initialize + authenticate 握手。**initialize/authenticate 失败一律归
    /// [`DshAcpError::Channel`]**（握手没成，谈不上方法语义；authenticate 恒成功系
    /// 源码直读——失败即通道异常面）
    pub fn start(spawn: &SpawnSeam, spec: &SpawnSpec) -> Result<Self, DshAcpError> {
        let io = spawn(spec).map_err(DshAcpError::Channel)?;
        let mut s = Self {
            io,
            next_id: 1,
            notifications: Vec::new(),
            permission_requests: Vec::new(),
            server_requests_other: Vec::new(),
            unknown_frames: 0,
            stray_responses: 0,
            agent_name: None,
            prompt_image_capability: None,
        };
        // initialize（ACP v1：protocolVersion + clientCapabilities——MAM 不提供 fs 面，
        // 全 false = 诚实最小声明；探测脚本发过 fs 形状但旗值未钉死，M25 复核）
        let result = s
            .call(
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": {
                        "fs": {"listRoots": false, "readTextFile": false, "writeTextFile": false}
                    }
                }),
            )
            .map_err(|e| {
                DshAcpError::Channel(format!("ACP_INITIALIZE_FAIL: initialize 失败：{e}"))
            })?;
        s.agent_name = result
            .get("agentInfo")
            .and_then(|a| a.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        // image 位随 provider/model 动态（T0.4 §2.1）——登记透出，不缓存消费
        s.prompt_image_capability = result
            .get("agentCapabilities")
            .and_then(|c| c.get("promptCapabilities"))
            .and_then(|p| p.get("image"))
            .and_then(serde_json::Value::as_bool);
        // authenticate（恒成功免鉴权——源码直读；失败即通道异常面，谈不上方法语义）
        s.call("authenticate", serde_json::json!({})).map_err(|e| {
            DshAcpError::Channel(format!("ACP_AUTHENTICATE_FAIL: authenticate 失败：{e}"))
        })?;
        Ok(s)
    }

    /// 下一个请求 id
    fn next_req_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// 写出请求帧（带 `jsonrpc:"2.0"`——标准 ACP 方言；解析侧宽松不依赖）
    fn send_request_frame(
        &mut self,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), DshAcpError> {
        // 请求帧带 jsonrpc:"2.0"（标准 ACP 方言）；解析侧宽松不依赖（见 parse_frame）
        let frame =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let line = serde_json::to_string(&frame).map_err(|e| {
            DshAcpError::Channel(format!("ACP_REQUEST_SERIALIZE_FAIL: 请求序列化失败：{e}"))
        })?;
        self.io.send(&line).map_err(DshAcpError::Channel)
    }

    /// 读一行（EOF/IO 错误 → Channel，附调用上下文）
    fn recv_line(&mut self, ctx: &str, id: u64) -> Result<String, DshAcpError> {
        match self.io.recv() {
            Some(Ok(l)) => Ok(l),
            Some(Err(e)) => Err(DshAcpError::Channel(format!(
                "ACP_RESPONSE_READ_FAIL: {ctx}(id={id}) 读取失败：{e}"
            ))),
            None => Err(DshAcpError::Channel(format!(
                "ACP_EOF_BEFORE_RESPONSE: 连接在 {ctx}(id={id}) 响应前关闭（EOF）"
            ))),
        }
    }

    /// 旁流帧分道收集（响应等待窗内出现的非匹配帧）
    fn collect_side_frame(&mut self, frame: WireFrame) {
        match frame {
            WireFrame::Response { .. } => self.stray_responses += 1,
            WireFrame::Notification { method, .. } => self.notifications.push(method),
            WireFrame::ServerRequest { id, method, params } => {
                self.collect_server_request(id, &method, params);
            }
            WireFrame::Unknown => self.unknown_frames += 1,
        }
    }

    /// 服务端请求分道：`session/request_permission` → 收集（只展示不代答——代答列 P2）；
    /// 其余旁记（不猜不代答）
    fn collect_server_request(&mut self, id: u64, method: &str, params: serde_json::Value) {
        if method == "session/request_permission" {
            self.permission_requests
                .push(PermissionRequest::from_frame(id, &params));
        } else {
            self.server_requests_other.push((id, method.to_string()));
        }
    }

    /// 通用请求-响应调用（initialize/authenticate/list/resume/new/close/set_config 用）
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, DshAcpError> {
        let id = self.next_req_id();
        self.send_request_frame(id, method, params)?;
        loop {
            let line = self.recv_line(method, id)?;
            match parse_frame(&line) {
                WireFrame::Response { id: rid, payload } if rid == id => match payload {
                    FramePayload::Ok(v) => return Ok(v),
                    FramePayload::Err(e) => return Err(classify(method, e)),
                },
                // 等待窗内的旁流帧分道收集（他人 id 响应只旁记计数——不匹配不解释）
                other => self.collect_side_frame(other),
            }
        }
    }

    /// 写出通知帧（无 id——cancel 专用形态）
    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), DshAcpError> {
        // notification 帧：无 id（cancel 专用形态——无响应帧可核对）
        let frame = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        let line = serde_json::to_string(&frame).map_err(|e| {
            DshAcpError::Channel(format!("ACP_NOTIFY_SERIALIZE_FAIL: 通知序列化失败：{e}"))
        })?;
        self.io.send(&line).map_err(DshAcpError::Channel)
    }

    /// cwd 合法性（投递前拒——探测定案：相对路径被服务端 `-32602` 拒；MAM 侧零协议副作用先拒）
    fn ensure_absolute_cwd(cwd: &str) -> Result<(), DshAcpError> {
        // 探测定案（T0.4 §2.2）：相对路径被服务端 -32602 拒——MAM 侧投递前先拒（零协议副作用）
        if !std::path::Path::new(cwd).is_absolute() {
            return Err(DshAcpError::InvalidTarget(format!(
                "DSH_ACP_CWD_NOT_ABSOLUTE: cwd 必须是绝对路径（相对路径会被服务端拒绝——投递前拒）：{cwd}"
            )));
        }
        Ok(())
    }

    /// `session/list`（cwd 筛选 + keyset-cursor 翻页到尽头；页大小由服务端 config 决定，
    /// 客户端无 limit 参数——T0.4 §2.2）。护栏 = [`MAX_LIST_PAGES`]
    pub fn list_sessions(&mut self, cwd: &str) -> Result<Vec<SessionEntry>, DshAcpError> {
        self.list_sessions_with_budget(cwd, MAX_LIST_PAGES)
    }

    /// 分页护栏的可测面（Minor 9）：预算注入版——预算耗尽仍见 nextCursor = Channel
    /// 如实报（不静默截断）；生产入口 [`Self::list_sessions`] 用 [`MAX_LIST_PAGES`]
    fn list_sessions_with_budget(
        &mut self,
        cwd: &str,
        max_pages: usize,
    ) -> Result<Vec<SessionEntry>, DshAcpError> {
        Self::ensure_absolute_cwd(cwd)?;
        let mut all: Vec<SessionEntry> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..max_pages {
            let mut params = serde_json::Map::new();
            params.insert("cwd".to_string(), serde_json::json!(cwd));
            if let Some(c) = &cursor {
                params.insert("cursor".to_string(), serde_json::json!(c));
            }
            let result = self.call("session/list", serde_json::Value::Object(params))?;
            if let Some(arr) = result.get("sessions").and_then(serde_json::Value::as_array) {
                for v in arr {
                    // 条目形状 {sessionId, cwd}——两键缺一跳过（服务端本就排除 cwd 缺失条目，不猜）
                    if let (Some(sid), Some(c)) = (
                        v.get("sessionId").and_then(serde_json::Value::as_str),
                        v.get("cwd").and_then(serde_json::Value::as_str),
                    ) {
                        all.push(SessionEntry {
                            session_id: sid.to_string(),
                            cwd: c.to_string(),
                        });
                    }
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(serde_json::Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                return Ok(all); // 翻到尽头（keyset cursor 缺席/空 = 末页）
            }
        }
        Err(DshAcpError::Channel(format!(
            "DSH_ACP_PAGINATION_GUARD: session/list 分页护栏触发（> {max_pages} 页）——服务端页大小疑似失控，如实报不静默截断"
        )))
    }

    /// `session/resume`（仅非活跃会话；cwd 必填且须与持久化一致——探测 §2.3）。
    /// 活跃（桌面打开）→ [`DshAcpError::DesktopWriterActive`]；provider 不可用 →
    /// [`DshAcpError::ProviderUnavailable`]。响应回显 `configOptions` 原样透传
    /// （provider/model 面不接线——set_config_option 类型缝）
    pub fn resume_session(
        &mut self,
        session_id: &str,
        cwd: &str,
    ) -> Result<serde_json::Value, DshAcpError> {
        if session_id.trim().is_empty() {
            return Err(DshAcpError::InvalidTarget(
                "DSH_ACP_RESUME_SID_REQUIRED: resume 需要非空 sessionId（sessionId 二象性按原样回传——见模块头）"
                    .to_string(),
            ));
        }
        Self::ensure_absolute_cwd(cwd)?;
        // cwd 必填且必须与持久化 cwd 一致（探测 §2.3：validateWorkspaceParams + sameDirectory 双校验）
        self.call(
            "session/resume",
            serde_json::json!({"sessionId": session_id, "cwd": cwd}),
        )
        // 响应回显 configOptions（currentValue 为 JSON 字符串形态）——原样透传，provider/model 面不接线
    }

    /// `session/new`（`mcpServers` **必填**恒发 `[]`——比标准 ACP 严，探测 §2.4）。
    /// 返回 sessionId 为**裸 `<uuid>`**（无 `session-` 前缀）——原样透出绝不归一化
    pub fn new_session(&mut self, cwd: &str) -> Result<String, DshAcpError> {
        Self::ensure_absolute_cwd(cwd)?;
        let result = self.call(
            // mcpServers 必填（比标准 ACP 严——探测 §2.4）：MAM 无头不带 MCP 服务器
            "session/new",
            serde_json::json!({"cwd": cwd, "mcpServers": []}),
        )?;
        // 返回裸 <uuid>（无 session- 前缀）——按原样透出，绝不归一化（sessionId 二象性）
        result
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                DshAcpError::Channel(
                    "session/new 响应缺 sessionId 键（协议形状漂移，不猜）".to_string(),
                )
            })
    }

    /// `session/prompt` + update 流归一（完成判据 = 响应 `{stopReason}`）。
    /// 等待窗内 `session/update` 进累积器、`session/request_permission` 进收集（不代答）。
    /// 回合失败（如 `Insufficient Balance`）= `-32603` 帧错误 → [`DshAcpError::Rpc`]
    /// 携服务端原文
    pub fn prompt(&mut self, session_id: &str, text: &str) -> Result<PromptOutcome, DshAcpError> {
        let t0 = std::time::Instant::now();
        let id = self.next_req_id();
        self.send_request_frame(
            id,
            "session/prompt",
            serde_json::json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}]
            }),
        )?;
        let mut updates = UpdateStreamSummary::default();
        loop {
            let line = self.recv_line("session/prompt", id)?;
            match parse_frame(&line) {
                WireFrame::Response { id: rid, payload } if rid == id => {
                    let duration_ms = t0.elapsed().as_millis() as u64;
                    return match payload {
                        // 完成判据 = {stopReason}（在场透传——值域未验，M25 兜底）
                        FramePayload::Ok(v) => Ok(PromptOutcome {
                            stop_reason: v
                                .get("stopReason")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            updates,
                            duration_ms,
                        }),
                        // 回合失败（-32603 + 服务端原文，如 Insufficient Balance）→ Rpc 透传
                        FramePayload::Err(e) => Err(classify("session/prompt", e)),
                    };
                }
                // update 流归一（等待窗内逐帧累积——按会话串行、outputTail 保序，源码形状）
                WireFrame::Notification { method, params } if method == "session/update" => {
                    updates.push(parse_update(&params));
                }
                // 权限请求/其余旁流照分道收集（request_permission 只收集不代答）
                other => self.collect_side_frame(other),
            }
        }
    }

    /// `session/cancel`（**notification 形态**——无响应帧可核对；语义 = 取消进行中
    /// prompt，未知 sessionId 空操作、无 prompt 时取消自主工作——皆无回执，如实注记）。
    /// 结算以在飞 prompt 响应的 stopReason 为准（效果待 M25 真机验证）
    pub fn cancel(&mut self, session_id: &str) -> Result<CancelNotice, DshAcpError> {
        if session_id.trim().is_empty() {
            return Err(DshAcpError::InvalidTarget(
                "DSH_ACP_CANCEL_SID_REQUIRED: cancel 需要非空 sessionId（未知 sessionId 为空操作——也须是显式选择，不猜）"
                    .to_string(),
            ));
        }
        self.notify(
            "session/cancel",
            serde_json::json!({"sessionId": session_id}),
        )?;
        Ok(CancelNotice {
            session_id: session_id.to_string(),
            sent: true,
            note: CANCEL_NOTE.to_string(),
        })
    }

    /// `session/close`（释放写锁——**每会话必收尾**；探测 §2.4：`{}` 响应 ~4ms）
    pub fn close_session(&mut self, session_id: &str) -> Result<(), DshAcpError> {
        // {} 响应即释放写锁（探测 §2.4：~4ms）；失败如实 Err（send 编排里尽力收尾不掩盖回合结果）
        self.call(
            "session/close",
            serde_json::json!({"sessionId": session_id}),
        )
        .map(|_| ())
    }

    /// `session/set_config_option`——**类型缝，不接线**（send/dispatch 不调它）。
    /// `value` 必须是 **JSON 字符串形态**（传数组被 schema 拒——探测 §2.4；
    /// 如 model 档值 = `"[\"deepseek-official\",\"deepseek-v4-flash\"]"`）
    pub fn set_config_option(
        &mut self,
        session_id: &str,
        name: &str,
        value_json: &str,
    ) -> Result<serde_json::Value, DshAcpError> {
        // value = JSON 字符串形态（类型缝——send/dispatch 不调它；接线时保持字符串形态）
        self.call(
            "session/set_config_option",
            serde_json::json!({"sessionId": session_id, "name": name, "value": value_json}),
        )
    }

    /// 优雅退出（关 stdin 等自然退出——见 [`AcpIo::finish`]）
    pub fn exit(&mut self) {
        self.io.finish();
    }
}

// ============================================================
// per-action 客户端：三态靶向发送（裁决 32a 同款短命会话）
// ============================================================

/// 发送靶向（三态的实际走向——回执/审计据此标注，Task 10 消费）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Targeting {
    /// ① 命中且 resume 成功——精确靶向既有会话（比 WB 强的差异化能力）
    Resumed { session_id: String },
    /// ② 命中但桌面写者占用——如实拒（零字节投递；不做 queue 臆测）
    RefusedDesktopWriter {
        session_id: String,
        lock_message: String,
    },
    /// ③ 无既有会话——新建后投递（MAM 无头新建语义同族）
    NewSession { session_id: String },
}

impl Targeting {
    /// 实际走向的会话号（审计可追溯）
    pub fn session_id(&self) -> &str {
        match self {
            Targeting::Resumed { session_id }
            | Targeting::RefusedDesktopWriter { session_id, .. }
            | Targeting::NewSession { session_id } => session_id,
        }
    }
}

/// 靶向词表（评审 I6；与 WB [`super::wb_acp::TARGETING_PREFIX`] **同纪律、各自措辞**
/// ——两家的「新建/当前会话」语义不同，共用一句话会说错话）。
pub const TARGETING_PREFIX: &str = "已投递至 dsh 新建会话";
/// 不一致明示语（投递会话 ≠ 卡面所选会话——用户以为发给了 A，实际进了新建的 B）
pub const MISMATCH_CLAUSE: &str = "消息已进入新建会话，与所选会话不同";

/// new 臂靶向标注（对齐 WB [`super::wb_acp::delivered_session_note`] 的语义，评审 I6）。
///
/// `Some(全文)` = **必须明示**：dsh 的 resume 未命中时新建会话投递
/// （[`Targeting::NewSession`]）——投递目标 ≠ 卡面所选会话是**结构性**风险（不是异常），
/// 回执必须自己说清「进了哪个会话、与所选不同」，否则用户只看到一个裸 `ok`。
/// `None` = 无需标注（投递会话号为空 / 与卡面一致）。
pub fn delivered_session_note(delivered_sid: &str, card_sid: &str) -> Option<String> {
    if delivered_sid.is_empty() || delivered_sid == card_sid {
        return None;
    }
    Some(format!(
        "DSH_ACP_TARGETING_NOTE: {TARGETING_PREFIX}（sessionId={delivered_sid}）；注意：{MISMATCH_CLAUSE}"
    ))
}

/// 审计 result 词的靶向双记（对齐 WB [`super::wb_acp::audit_word_with_targeting`]，评审 I6）：
/// 审计行的 session 列 = **卡面会话**（设备检索入口语义：用户对哪张卡做了动作，不动）；
/// 不一致时把**实际投递会话号**双记进 result 词——两个会话都可追溯，不二选一。
pub fn audit_word_with_targeting(word: &str, card_sid: &str, delivered_sid: &str) -> String {
    if delivered_sid.is_empty() || delivered_sid == card_sid {
        return word.to_string();
    }
    format!("{word}（{TARGETING_PREFIX} sessionId={delivered_sid}，{MISMATCH_CLAUSE}）")
}

/// 回执靶向明示的应用单点（评审 I6；api.rs `run_dsh_turn` 调用）：`NewSession` 且投递
/// 会话 ≠ 卡面会话时，把明示语**追加**到 `receipt.reason`（原有成因保留在前，明示语在后
/// ——与 WB 三态同款纪律）。
///
/// **Failed 不加**（对齐 WB `delivered_session_note` 的边界）：协议错误/通道错误下正文
/// **未必送达**，此时说「消息已进入新建会话」是超出证据的结论；投递会话号仍由回执
/// `session_id` 字段承载（可追溯性不丢）。返回是否实际追加（可观测面）。
pub fn apply_targeting_note(receipt: &mut Receipt, targeting: &Targeting, card_sid: &str) -> bool {
    if receipt.status == ReceiptStatus::Failed {
        return false;
    }
    let Targeting::NewSession { session_id } = targeting else {
        return false;
    };
    let Some(note) = delivered_session_note(session_id, card_sid) else {
        return false;
    };
    let tail = receipt.reason.take().unwrap_or_default();
    receipt.reason = Some(if tail.is_empty() {
        note
    } else {
        format!("{tail}；{note}")
    });
    true
}

/// dsh 发送回执（归一 Receipt + 靶向标注 + 完成判据透传）
#[derive(Debug, Clone, PartialEq)]
pub struct DshSendReceipt {
    pub targeting: Targeting,
    /// 归一回执（成功 = 末条 assistant 摘要 + 回合时长〔turn.rs 底座单点口径〕；
    /// 桌面占用 = Failed/Refused；回合失败 = Failed/ChannelError 携服务端原文）
    pub receipt: Receipt,
    /// prompt 完成判据（在场透传——值域未验 M25 兜底；拒/失败路径 = None）
    pub stop_reason: Option<String>,
    pub updates: UpdateStreamSummary,
    /// 等待窗内收集的权限请求（**透传展示面**——Task 10 走 send() 主入口即可达；
    /// 只收集不代答，代答列 P2〔本批-3〕）
    pub permission_requests: Vec<PermissionRequest>,
}

/// per-action 短命客户端：每动作 spawn → initialize+authenticate → 调用 → close → 退出
pub struct DshAcpClient {
    spawn: SpawnSeam,
    spec: SpawnSpec,
}

impl DshAcpClient {
    /// `spawn` = 执行缝（测试桩 / 生产 [`production_spawn_seam`]）；`spec` = spawn 规格
    pub fn new(spawn: SpawnSeam, spec: SpawnSpec) -> Self {
        Self { spawn, spec }
    }

    /// 起一枚短命会话（spawn + initialize + authenticate）；调用方负责 close+exit。
    /// **同步阻塞**（握手含两次请求-响应往返）——async 上下文须放 `spawn_blocking`
    /// （模块头「调用契约」）
    pub fn session(&self) -> Result<DshAcpSession, DshAcpError> {
        DshAcpSession::start(&self.spawn, &self.spec)
    }

    /// 三态靶向发送（T0.4 定案编排）：cwd 绝对路径校验（投递前拒，零 spawn）→
    /// `session/list`（cwd 筛选+分页）→ ①命中 → resume → prompt → close；
    /// ②resume 被桌面写者拒 → 如实拒回执（Stage::Refused——会话被占用的零字节投递面）；
    /// ③未命中/空目标 → `session/new` → prompt → close。回合失败（`Insufficient Balance`
    /// 一类）归一回执面（ChannelError 携服务端原文）；通道级失败如实 Err。
    /// provider 不可用（事后判）如实 Err——Task 10 可据此换候选或降级展示。
    /// **同步阻塞**（prompt 等待窗最长 = 回合时长，单帧读预算 [`PROC_READ_TIMEOUT_MS`]）
    /// ——async 上下文须放 `spawn_blocking`，绝不在 async 运行时直呼（模块头「调用契约」）
    pub fn send(
        &self,
        target_session_id: &str,
        cwd: &str,
        text: &str,
    ) -> Result<DshSendReceipt, DshAcpError> {
        // 投递前拒（零 spawn——空文本/非绝对 cwd 不烧协议面）
        if text.trim().is_empty() {
            return Err(DshAcpError::InvalidTarget(
                "DSH_ACP_EMPTY_TEXT: 投递文本为空——投递前拒（零协议副作用）".to_string(),
            ));
        }
        DshAcpSession::ensure_absolute_cwd(cwd)?;
        let target = target_session_id.trim();
        let mut s = self.session()?;
        // 三态靶向（T0.4 定案编排）：list[cwd 筛选+分页] → 命中 → resume（写者锁在此
        // 暴露）→ prompt；未命中/无目标 → new。provider 预筛只能事后（list 条目无
        // provider 字段——模块头登记）。
        enum Route {
            Resumed(String),
            New(String),
        }
        let route = if target.is_empty() {
            // 无目标 id：list 无从匹配，直达 ③（MAM 无头新建语义同族）
            Route::New(s.new_session(cwd)?)
        } else {
            let entries = s.list_sessions(cwd)?;
            match entries.iter().find(|e| e.session_id == target) {
                None => Route::New(s.new_session(cwd)?),
                Some(entry) => match s.resume_session(&entry.session_id, cwd) {
                    Ok(_) => Route::Resumed(entry.session_id.clone()),
                    Err(DshAcpError::DesktopWriterActive { lock_message }) => {
                        // ② 单写者互斥如实拒（零字节投递；不做 queue 臆测——README 未提 queue）。
                        // 未持有写者：不发 session/close，直接优雅退出
                        // 等待窗（list/resume）内若有权限请求也如实透出（close/exit 前取出）
                        let permission_requests = s.permission_requests.clone();
                        s.exit();
                        return Ok(DshSendReceipt {
                            targeting: Targeting::RefusedDesktopWriter {
                                session_id: entry.session_id.clone(),
                                lock_message,
                            },
                            receipt: Receipt::failed(Stage::Refused, ACTIVE_SESSION_REFUSAL)
                                .with_session(&entry.session_id),
                            stop_reason: None,
                            updates: UpdateStreamSummary::default(),
                            permission_requests,
                        });
                    }
                    // provider 不可用（事后判）等其余错误——如实 Err（Task 10 可据此换候选/降级）
                    Err(e) => {
                        s.exit();
                        return Err(e);
                    }
                },
            }
        };
        let (targeting, sid) = match route {
            Route::Resumed(sid) => (
                Targeting::Resumed {
                    session_id: sid.clone(),
                },
                sid,
            ),
            Route::New(sid) => (
                Targeting::NewSession {
                    session_id: sid.clone(),
                },
                sid,
            ),
        };
        // prompt（持写者状态）→ close（释放写锁——每会话必收尾）→ 优雅退出。
        // 权限请求在 close/exit 前取出装填（Important 1：send() 主入口透传展示面）
        let t0 = std::time::Instant::now();
        let prompt_result = s.prompt(&sid, text);
        let permission_requests = s.permission_requests.clone();
        match prompt_result {
            Ok(outcome) => {
                let _ = s.close_session(&sid);
                s.exit();
                Ok(DshSendReceipt {
                    receipt: delivered_receipt(&sid, &outcome),
                    stop_reason: outcome.stop_reason.clone(),
                    updates: outcome.updates.clone(),
                    targeting,
                    permission_requests,
                })
            }
            Err(DshAcpError::Rpc { code, message }) => {
                // 回合失败（服务端已应答错误——如 Insufficient Balance）：归一回执面，
                // 服务端原文透传（通道本身是好的，不冒充通道死、也不谎报成功）。
                // 协议层仍是干净收发——close 不险（与通道级失败区分，见下一臂）
                let _ = s.close_session(&sid);
                s.exit();
                Ok(DshSendReceipt {
                    receipt: Receipt::failed(
                        Stage::ChannelError,
                        &format!("DSH_ACP_TURN_RPC_FAIL: 回合失败（协议错误 {code}）：{message}"),
                    )
                    .with_session(&sid)
                    .with_duration_ms(t0.elapsed().as_millis() as u64),
                    stop_reason: None,
                    updates: UpdateStreamSummary::default(),
                    targeting,
                    permission_requests,
                })
            }
            // 通道级失败（EOF/IO/读超时/方法缺席）——如实 Err。**跳过 close 直接退出**
            // （spec m5：prompt 已阻塞过一个 300s 读预算，close 再等响应 = 再赌一个
            // 读预算；进程退出即弃连接，写锁随进程消亡由服务端回收）
            Err(e) => {
                s.exit();
                Err(e)
            }
        }
    }
}

/// 成功投递回执：末条 assistant 摘要走 turn.rs 底座单点口径（截断语义不另造一份）；
/// 空消息流 = 不带 lastAssistant（不猜回合产出）；tokens 恒 None（usage_update 是
/// 上下文窗口占用不是输出 token——不冒充，见模块头）
fn delivered_receipt(session_id: &str, outcome: &PromptOutcome) -> Receipt {
    match outcome.updates.last_message.as_deref() {
        Some(t) if !t.trim().is_empty() => {
            ok_receipt_with_assistant(session_id, t, None, outcome.duration_ms)
        }
        _ => Receipt::ok(session_id, outcome.duration_ms),
    }
}

// ============================================================
// 生产出口（cfg(not(test))——测试构建零真实进程；验证面 = M25 真机）
// ============================================================

/// 生产读预算：单次 recv 等待上限（回合级等待；与 codex_app 同款 300s watchdog 量级）
#[cfg(not(test))]
pub const PROC_READ_TIMEOUT_MS: u64 = 300_000;
/// 优雅退出宽限：关 stdin 后等自然退出的窗口；超窗才 kill 兜底（持写者期间 kill 是
/// 最后手段——探测 §1.2：stdin 关闭后 2s 内 exit 0，5s 宽限足够）
#[cfg(not(test))]
pub const FINISH_GRACE_MS: u64 = 5_000;

/// dsh 自带 runtime node 路径（T0.3 钉值：**不用系统 node**；探测为 Windows 形态，
/// mac 形态未取证——如实登记，非 Windows 平台 dsh 轨走不通时由调用方降级）
#[cfg(not(test))]
pub fn runtime_node_exe(dsh_home: &std::path::Path) -> std::path::PathBuf {
    // T0.3 钉值：dsh 自带 runtime node（探测为 Windows 形态；mac 未取证——模块头已登记）
    dsh_home
        .join("dsh-runtimes")
        .join("dsh-primary-runtime")
        .join("dependencies")
        .join("node")
        .join("bin")
        .join(if cfg!(windows) { "node.exe" } else { "node" })
}

/// desktop 安装的 asar 路径（`<install>/resources/app.asar`）
#[cfg(not(test))]
pub fn asar_path(install_dir: &std::path::Path) -> std::path::PathBuf {
    install_dir.join("resources").join("app.asar")
}

/// asar 指纹（size+mtime——[`extract_fingerprint`] 的生产装配）
#[cfg(not(test))]
pub fn asar_fingerprint(asar: &std::path::Path) -> Option<String> {
    let md = std::fs::metadata(asar).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(extract_fingerprint(md.len(), mtime))
}

/// 首启幂等解包（策略见模块头「asar 解包策略」）：digest 标记逐字相等 + bin.js 在场
/// → 跳过；否则经 Rust 最小 asar 读取器解 `\dsh` 子树并写标记。真机行为 M25 验证
#[cfg(not(test))]
/// 解包互斥（Minor 7）：双动作首启并发时，两个 ensure_extracted 会对同一缓存目录
/// 交叉写（半成品树 + 标记互踩）——进程内串行化；跨进程竞态由「marker 逐字比对 +
/// bin.js 在场」的幂等判据与单写者文件系统语义兜底
#[cfg(not(test))]
static EXTRACT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(not(test))]
pub fn ensure_extracted(
    install_dir: &std::path::Path,
    cache_root: &std::path::Path,
    desktop_version: &str,
) -> Result<std::path::PathBuf, String> {
    // 中毒锁也照常进入（poison 只说明持锁线程 panic 过——目录状态由幂等判据自愈）
    let _extract_guard = EXTRACT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let asar = asar_path(install_dir);
    let fingerprint = asar_fingerprint(&asar)
        .ok_or_else(|| format!("DSH_ACP_ASAR_UNREADABLE: asar 不可读：{}", asar.display()))?;
    let dest_root = cache_root.join(sanitize_version(desktop_version));
    let bin = {
        // 布局单点 = EXTRACT_SUBTREE_REL（与纯核 bin_js_path 同一真相源）
        let mut p = dest_root.clone();
        for part in EXTRACT_SUBTREE_REL.split('/') {
            p = p.join(part);
        }
        p
    };
    let marker = dest_root.join(EXTRACT_MARKER_FILE);
    let marker_now = std::fs::read_to_string(&marker).ok();
    match extract_decision(marker_now.as_deref(), &fingerprint) {
        ExtractDecision::Skip if bin.exists() => {}
        _ => {
            // 首启/指纹漂移（asar 升级）→ 解包 \dsh 子树 + 写标记（重跑同版本即 Skip）
            std::fs::create_dir_all(&dest_root)
                .map_err(|e| format!("DSH_ACP_CACHE_DIR_FAIL: 建缓存目录失败：{e}"))?;
            asar_extract::extract_subtree(&asar, &dest_root, "dsh").map_err(|e| {
                format!(
                    "DSH_ACP_ASAR_EXTRACT_FAIL: asar 解包失败（{}）：{e}",
                    asar.display()
                )
            })?;
            if !bin.exists() {
                return Err(format!(
                    "DSH_ACP_BIN_JS_MISSING: 解包后 bin.js 不在场（子树形状漂移，不猜）：{}",
                    bin.display()
                ));
            }
            std::fs::write(&marker, &fingerprint).map_err(|e| {
                format!("DSH_ACP_DIGEST_MARKER_WRITE_FAIL: 写 digest 标记失败：{e}")
            })?;
        }
    }
    Ok(bin)
}

/// 生产 spawn 缝：stdio 子进程（stdin/stdout 管道 + 显式 env 叠加；
/// **stderr = null**——探测定案零输出，且 piped 不读有管道缓冲死锁风险；
/// Windows `CREATE_NO_WINDOW` 防闪窗，仓内先例 monitor/git.rs）。**`cfg(not(test))`**：
/// 测试构建本函数不存在 ⇒ 单测无法触达真实进程（宪法级纪律）
#[cfg(not(test))]
pub fn production_spawn_seam() -> SpawnSeam {
    Arc::new(|spec: &SpawnSpec| {
        let program = spec
            .argv
            .first()
            .ok_or_else(|| "spawn 规格缺 argv[0]".to_string())?;
        let mut cmd = std::process::Command::new(program);
        cmd.args(&spec.argv[1..]);
        // 显式 env 叠加（DSH_HOME 透传 + 关遥测）；其余 env 继承父进程
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // stderr 零输出（探测定案）——用 null 而非 piped：piped 不读有管道缓冲死锁风险
            .stderr(std::process::Stdio::null());
        // Windows 下 GUI 进程 spawn 控制台程序会闪黑窗（仓内先例 monitor/git.rs 同款）
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
        let program = program.to_string();
        ProcIo::new(child, stdin, stdout, program).map(|io| Box::new(io) as Box<dyn AcpIo>)
    })
}

/// 生产传输面：读线程 + 超时预算 + 优雅收尾（实现细节，只被 production_spawn_seam 构造）
#[cfg(not(test))]
struct ProcIo {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    rx: std::sync::mpsc::Receiver<Result<String, String>>,
    finished: bool,
    /// 程序名（kill 兜底留痕用——照 codex_app.rs ProcIo::program 先例）
    program: String,
}

#[cfg(not(test))]
impl ProcIo {
    fn new(
        child: std::process::Child,
        stdin: std::process::ChildStdin,
        stdout: std::process::ChildStdout,
        program: String,
    ) -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("dsh-acp-io".to_string())
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
        Ok(Self {
            child,
            stdin: Some(stdin),
            rx,
            finished: false,
            program,
        })
    }
}

#[cfg(not(test))]
impl AcpIo for ProcIo {
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
                "DSH_ACP_READ_TIMEOUT: dsh-acp {}ms 无输出（读取超时）",
                PROC_READ_TIMEOUT_MS
            ))),
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.stdin.take(); // drop 关闭 stdin → 进程自然退出（探测 §1.2：2s 内 exit 0）
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(FINISH_GRACE_MS);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                // 宽限尽仍未退：kill 兜底（持写者期间 kill 是最后手段——模块头登记）。
                // Important 2：必须留痕（program + 宽限值）——静默 kill 会让「写者锁
                // 滞留」不可追溯（照 codex_app.rs kill 兜底 warn 先例；本通道 stderr =
                // null 无尾行可带）
                Ok(None) => {
                    log::warn!(
                        "dsh-acp（{}）优雅退出宽限 {}ms 已尽仍未退出——kill 兜底（持写者时写者锁可能滞留；探测 §1.2：stdin 关闭后正常应 2s 内退出）",
                        self.program,
                        FINISH_GRACE_MS
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
        self.finish(); // 调用方忘 exit 也优雅收尾（防进程泄漏）
    }
}

/// 最小 asar 读取器（cfg(not(test))——纯核不依赖它；格式 = 8 字节 size-pickle 头 +
/// pickle JSON 头 + 连续 blob，按 offset/size 拷文件；unpacked 条目从伴生目录拷）
#[cfg(not(test))]
mod asar_extract {
    use std::io::{Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};

    /// 解包 asar 内某个顶层子树（如 `dsh`）到 `<dest_root>/<top>/`
    pub fn extract_subtree(asar: &Path, dest_root: &Path, top: &str) -> Result<(), String> {
        let mut f = std::fs::File::open(asar)
            .map_err(|e| format!("DSH_ACP_ASAR_OPEN_FAIL: 打开 asar 失败：{e}"))?;
        // 头部布局：[8 字节 size-pickle（4B 载荷长=4 + 4B 头块长 H）][H 字节头块
        // （4B JSON 串长 L + L 字节 JSON + padding）][连续文件 blob]——文件绝对偏移 =
        // 8 + H + entry.offset（offset 为十进制字符串，相对 data 区起点）
        let mut head8 = [0u8; 8];
        f.read_exact(&mut head8)
            .map_err(|e| format!("DSH_ACP_ASAR_HEADER_READ_FAIL: 读 asar 头失败：{e}"))?;
        let payload_len = u32::from_le_bytes(head8[0..4].try_into().unwrap());
        if payload_len != 4 {
            return Err(format!(
                "DSH_ACP_ASAR_HEADER_SHAPE: asar 头形状异常（size-pickle 载荷长 {payload_len} != 4）：{}",
                asar.display()
            ));
        }
        let header_size = u64::from(u32::from_le_bytes(head8[4..8].try_into().unwrap()));
        // spec M1：分配前核对——头块长必须落在文件内（防伪造/损坏头部的越界分配）
        let file_len = f
            .metadata()
            .map_err(|e| format!("DSH_ACP_ASAR_STAT_FAIL: 读 asar 元数据失败：{e}"))?
            .len();
        if 8 + header_size > file_len {
            return Err(format!(
                "DSH_ACP_ASAR_HEADER_SIZE_RANGE: asar 头块长越界（8+{header_size} > 文件长 {file_len}）：{}",
                asar.display()
            ));
        }
        let mut header_buf = vec![0u8; header_size as usize];
        f.read_exact(&mut header_buf)
            .map_err(|e| format!("DSH_ACP_ASAR_HEADER_BLOCK_READ_FAIL: 读 asar 头块失败：{e}"))?;
        let json_len = u32::from_le_bytes(header_buf[0..4].try_into().unwrap()) as usize;
        if 4 + json_len > header_buf.len() {
            return Err("DSH_ACP_ASAR_JSON_LEN_RANGE: asar 头 JSON 长度越界".to_string());
        }
        let header: serde_json::Value = serde_json::from_slice(&header_buf[4..4 + json_len])
            .map_err(|e| format!("DSH_ACP_ASAR_JSON_PARSE_FAIL: asar 头 JSON 解析失败：{e}"))?;
        let top_files = header
            .get("files")
            .and_then(|fs| fs.get(top))
            .and_then(|t| t.get("files"))
            .ok_or_else(|| format!("DSH_ACP_ASAR_SUBTREE_MISSING: asar 无 {top} 目录子树"))?;
        let data_base = 8 + header_size;
        walk(
            &mut f,
            data_base,
            file_len,
            top_files,
            &dest_root.join(top),
            &unpacked_root(asar),
            top,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        f: &mut std::fs::File,
        data_base: u64,
        file_len: u64,
        files: &serde_json::Value,
        dir_dest: &Path,
        unpacked_root: &Path,
        rel: &str,
    ) -> Result<(), String> {
        use serde_json::Value;
        std::fs::create_dir_all(dir_dest)
            .map_err(|e| format!("DSH_ACP_MKDIR_FAIL: 建目录失败 {}: {e}", dir_dest.display()))?;
        let obj = files
            .as_object()
            .ok_or_else(|| "DSH_ACP_ASAR_FILES_NOT_OBJECT: asar files 节非对象".to_string())?;
        for (name, entry) in obj {
            // 防路径穿越（spec m1 补全）：条目名必须是纯文件名——`/`、`\`、
            // `:`（盘符/NTFS 流分隔）、`.`/`..`（当前/父目录）一律拒——不猜
            if name.contains('/')
                || name.contains('\\')
                || name.contains(':')
                || name == "."
                || name == ".."
            {
                return Err(format!(
                    "DSH_ACP_ASAR_ENTRY_NAME_UNSAFE: asar 条目名异常（疑似路径注入）：{name}"
                ));
            }
            let child_dest = dir_dest.join(name);
            let child_rel = format!("{rel}/{name}");
            if let Some(sub) = entry.get("files") {
                walk(
                    f,
                    data_base,
                    file_len,
                    sub,
                    &child_dest,
                    unpacked_root,
                    &child_rel,
                )?;
            } else {
                // Minor 6（spec m2）：symlink 条目专属错误——pnpm 布局常带链接，
                // 静默跳过 = 运行期缺依赖难诊断；如实报「暂不支持 + 指向」
                if let Some(target) = entry.get("link").and_then(Value::as_str) {
                    return Err(format!(
                        "DSH_ACP_SYMLINK_UNSUPPORTED: symlink 条目暂不支持（M25 真机核对后定夺）：{child_rel} -> {target}"
                    ));
                }
                let size = entry.get("size").and_then(Value::as_u64).ok_or_else(|| {
                    format!("DSH_ACP_ASAR_SIZE_MISSING: asar 文件条目缺 size：{child_rel}")
                })?;
                if entry.get("unpacked").and_then(Value::as_bool) == Some(true) {
                    // unpacked 条目：内容不在 asar 内，在 <asar>.unpacked/<rel> 伴生树
                    let src = unpacked_root.join(&child_rel);
                    std::fs::copy(&src, &child_dest).map_err(|e| {
                        format!("DSH_ACP_UNPACKED_COPY_FAIL: 拷 unpacked 条目失败 {child_rel}：{e}")
                    })?;
                } else {
                    let off = entry
                        .get("offset")
                        .and_then(Value::as_str)
                        .and_then(|x| x.parse::<u64>().ok())
                        .ok_or_else(|| {
                            format!(
                                "DSH_ACP_ASAR_OFFSET_MISSING: asar 文件条目缺 offset：{child_rel}"
                            )
                        })?;
                    // spec M1：分配前核对——data_base + off + size 必须落在文件内
                    //（防伪造/损坏条目的巨型分配或越界读）
                    if data_base + off + size > file_len {
                        return Err(format!(
                            "DSH_ACP_ASAR_RANGE_OUT_OF_BOUNDS: asar 条目区间越界（data_base={data_base} off={off} size={size} > 文件长 {file_len}）：{child_rel}"
                        ));
                    }
                    let mut buf = vec![0u8; size as usize];
                    f.seek(SeekFrom::Start(data_base + off))
                        .map_err(|e| format!("DSH_ACP_SEEK_FAIL: seek 失败 {child_rel}：{e}"))?;
                    f.read_exact(&mut buf).map_err(|e| {
                        format!("DSH_ACP_ENTRY_READ_FAIL: 读条目失败 {child_rel}：{e}")
                    })?;
                    std::fs::write(&child_dest, &buf).map_err(|e| {
                        format!("DSH_ACP_ENTRY_WRITE_FAIL: 写条目失败 {child_rel}：{e}")
                    })?;
                }
                // 可执行位（unix 面；探测为 Windows 桌面形态——Windows 无此面）
                #[cfg(unix)]
                if entry.get("executable").and_then(Value::as_bool) == Some(true) {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &child_dest,
                        std::fs::Permissions::from_mode(0o755),
                    );
                }
            }
        }
        Ok(())
    }

    /// `<asar>.unpacked` 伴生目录（unpacked 条目的真实落点）
    fn unpacked_root(asar: &Path) -> PathBuf {
        // <asar>.unpacked（如 app.asar → app.asar.unpacked）
        let mut s = asar.as_os_str().to_os_string();
        s.push(".unpacked");
        PathBuf::from(s)
    }
}

// ============================================================
// 生产装配 + 测试钩子（Task 10 接线层）
// ============================================================

/// 每动作客户端装配（Task 10 端点消费；照 `wb_acp::deps_for` 先例 cfg 切换）：
/// - **生产**（`cfg(not(test))`）：真实发现链——`DSH_HOME`/home → runtime node →
///   `runtime.json` 的 `desktopVersion`（缓存键）→ asar 幂等解包 → argv/env 钉值；
/// - **测试**（`cfg(test)`）：缝来自 [`test_hooks`]（默认拒绝 spawn——零真实进程零
///   真实 `~/.dsh` 接触，宪法级纪律），不做任何 fs 发现。
///
/// **`home` 语义**：调用方（api.rs 端点）经 `RemoteState.home_source` 缝传入——
/// 生产 = 真实用户主目录；测试 = None/tempdir。缓存根 = `<home>/.tuvis/dsh-asar`
/// （MAM 数据目录同源口径；`DSH_HOME` env 优先级归 `monitor::dsh::dsh_home_with`
/// 单点，此处不另判）。
pub fn assemble_client(home: Option<std::path::PathBuf>) -> Result<DshAcpClient, String> {
    #[cfg(not(test))]
    {
        production_assemble(home)
    }
    #[cfg(test)]
    {
        // 测试构建：不做 fs 发现（零真实 ~/.dsh 接触）；规格是占位（缝桩不消费 spec）
        let _ = home;
        Ok(DshAcpClient::new(
            test_hooks::spawn(),
            SpawnSpec {
                argv: Vec::new(),
                env: Vec::new(),
            },
        ))
    }
}

#[cfg(not(test))]
fn production_assemble(home: Option<std::path::PathBuf>) -> Result<DshAcpClient, String> {
    // 1) DSH_HOME（env 优先 → home/.dsh；单源 = monitor::dsh::dsh_home_with）
    let dsh_home = crate::monitor::dsh::dsh_home_with(&home.clone().unwrap_or_default());
    // 2) runtime node（T0.3 钉值：dsh 自带 runtime，不用系统 node）——不在场即如实拒
    //    （安装未完成/未首启；零 spawn）
    let node = runtime_node_exe(&dsh_home);
    if !node.exists() {
        return Err(format!(
            "DSH_ACP_RUNTIME_NODE_MISSING: dsh runtime node 不可达（{}）——请先安装并启动一次 DeepSeek Harness 桌面端",
            node.display()
        ));
    }
    // 3) desktopVersion = 缓存键（runtime.json 与桌面同源更新；版本变 ⇒ 新缓存子目录
    //    ⇒ 自动重解包——这是「缓存优先可跳过安装发现」的自愈面）
    let runtime_json = dsh_home
        .join("dsh-runtimes")
        .join("dsh-primary-runtime")
        .join("runtime.json");
    let version = read_desktop_version(&runtime_json).ok_or_else(|| {
        format!(
            "DSH_ACP_DESKTOP_VERSION_UNREADABLE: 读不到 desktopVersion（{}）——无法定位 dsh-acp 解包缓存键",
            runtime_json.display()
        )
    })?;
    // 4) 缓存根（home 缺席即如实拒——缓存与 MAM 数据目录同源）
    let Some(home) = home else {
        return Err(
            "DSH_ACP_HOME_UNAVAILABLE: 用户主目录不可用——无法定位 dsh-asar 解包缓存".to_string(),
        );
    };
    let cache_root = home.join(".tuvis").join("dsh-asar");
    // 5) bin.js：缓存优先（幂等解包后常驻；此时无需安装目录在场）。未命中 → 安装发现
    //    （进程表 cmdline 的 app.asar 路径〔桌面在跑时最真〕→ 常见安装位置兜底）→ 解包
    let mut bin = {
        let mut p = cache_root.join(sanitize_version(&version));
        for part in EXTRACT_SUBTREE_REL.split('/') {
            p = p.join(part);
        }
        p
    };
    if !bin.exists() {
        let install_dir = discover_install_dir()
            .ok_or("未发现 DeepSeek Harness 安装（桌面端未在运行、常见安装位置均未命中）——无法解包 dsh-acp 入口")?;
        bin = ensure_extracted(&install_dir, &cache_root, &version)?;
    }
    // 6) argv/env 钉值（T0.3；不含 --patch——整对象替换红线见模块头）
    Ok(DshAcpClient::new(
        production_spawn_seam(),
        SpawnSpec {
            argv: acp_argv(&node.to_string_lossy(), &bin.to_string_lossy()),
            env: acp_env(&dsh_home.to_string_lossy()),
        },
    ))
}

/// `runtime.json` 的 `desktopVersion`（缓存键原料；防御抽取，缺场 = None 不猜）
#[cfg(not(test))]
fn read_desktop_version(runtime_json: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(runtime_json).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("desktopVersion")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// 安装目录发现（冷缓存时才走；桌面在跑看进程表最真，否则常见安装位置兜底）。
/// macOS 安装形态未取证（模块头登记）——发现失败由调用方如实拒，不假装支持
#[cfg(not(test))]
fn discover_install_dir() -> Option<std::path::PathBuf> {
    // ① 进程表：桌面宿主 cmdline 含 `…\resources\app.asar\dsh\…`（monitor::dsh
    //    实测取证形态）——特征判定复用单源 `cmdline_is_dsh_desktop_host`
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    for p in sys.processes().values() {
        let cmd = p.cmd();
        if cmd.is_empty() || !crate::monitor::dsh::cmdline_is_dsh_desktop_host(cmd) {
            continue;
        }
        for tok in cmd {
            let tok = tok.to_string_lossy();
            // 归一查 `resources[/\]app.asar` 的出现位置——前缀即安装目录
            let t = tok.replace('\\', "/");
            if let Some(idx) = t.find("/resources/app.asar") {
                let install = &t[..idx];
                if !install.is_empty() {
                    return Some(std::path::PathBuf::from(install));
                }
            }
        }
    }
    // ② 常见安装位置（NSIS/Electron 常规布局）：只做存在性探测（`resources/app.asar`
    //    在场即认），不做任何写动作。**机器特定兜底申报**：`C:\`/`D:\` 两条硬编码盘符
    //    来自本机实测形态（D:\Program Files；精简机器 env 三键可能全空）——env 键未命中
    //    时的最后兜底，非跨机器保证。**macOS 明确不兜底**（安装形态未取证，模块头登记）
    //    ——发现失败由调用方如实拒，不假装支持
    let mut roots: Vec<std::path::PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(std::path::PathBuf::from))
        .collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(std::path::PathBuf::from(local).join("Programs"));
    }
    roots.push(std::path::PathBuf::from("C:\\Program Files"));
    roots.push(std::path::PathBuf::from("D:\\Program Files"));
    for root in roots {
        let install = root.join("Deepseek-Harness");
        if install.exists() && asar_path(&install).exists() {
            return Some(install);
        }
    }
    None
}

/// **测试专用注入钩子**（仅 `cfg(test)`；照 `wb_acp::test_hooks` 先例）：端点级用例
/// 在测试构建里装配 spawn 缝（脚本桩），默认 = 拒绝 spawn（零真实进程纪律）。
/// **并行纪律**（钩子是进程级全局态）：凡读写钩子的用例必须持 [`test_hooks::LOCK`]。
#[cfg(test)]
pub mod test_hooks {
    use super::*;
    type SeamSlot = std::sync::LazyLock<std::sync::Mutex<Option<SpawnSeam>>>;
    static SEAM: SeamSlot = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

    /// 钩子用例串行锁（tokio 互斥量：守卫跨 await 持有，与 wb_acp::test_hooks::LOCK 同款）
    type HookLock = std::sync::LazyLock<tokio::sync::Mutex<()>>;
    pub(crate) static LOCK: HookLock = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    /// 装填 spawn 缝（**调用方必须已持 [`LOCK`]**）
    pub(crate) fn set_spawn(seam: SpawnSeam) {
        *SEAM.lock().unwrap_or_else(|e| e.into_inner()) = Some(seam);
    }

    /// 清空钩子（用例收尾；Drop 守卫调用）
    pub(crate) fn clear() {
        *SEAM.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// spawn 缝（默认：拒绝 spawn——测试构建绝不落真进程）
    pub(crate) fn spawn() -> SpawnSeam {
        if let Some(seam) = SEAM.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return seam;
        }
        Arc::new(|_spec: &SpawnSpec| {
            Err("测试构建未装填 dsh-acp spawn 缝（零进程纪律）".to_string())
        })
    }
}

// ============================================================
// tests（纯核：全部经缝 mock，零真实进程零网络——宪法级纪律）
// ============================================================

#[cfg(test)]
mod tests {
    use super::super::receipt::ReceiptStatus;
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ===== 靶向明示（评审 I6）=====

    /// new 臂靶向标注与审计双记的**词表与判据单点**（纯核）：投递会话 ≠ 卡面会话时必须
    /// 明示（含投递号 + 「与所选会话不同」）；一致/空号 → 无需标注（零冗余）。
    /// 撤回修复（把 `delivered_session_note` 恒回 None）→ 前两条断言先红。
    #[test]
    fn new_session_delivery_note_is_mandatory_when_targeting_mismatches() {
        const CARD: &str = "session-aaaa";
        const NEW: &str = "3fb0c1d2-aaaa-4bbb-8ccc-dddddddddddd";
        let note = delivered_session_note(NEW, CARD).expect("投递会话 ≠ 卡面 ⇒ 必须明示");
        assert!(note.contains(TARGETING_PREFIX), "{note}");
        assert!(note.contains(&format!("sessionId={NEW}")), "{note}");
        assert!(note.contains(MISMATCH_CLAUSE), "{note}");
        assert_eq!(
            delivered_session_note(CARD, CARD),
            None,
            "投递会话 = 卡面 ⇒ 零冗余（不标注）"
        );
        assert_eq!(
            delivered_session_note("", CARD),
            None,
            "空号 ⇒ 无投递会话可标"
        );
        // 审计双记：不一致时补投递视角；一致/空号时原词零变化
        assert_eq!(audit_word_with_targeting("ok", CARD, CARD), "ok");
        assert_eq!(audit_word_with_targeting("ok", CARD, ""), "ok");
        let word = audit_word_with_targeting("ok", CARD, NEW);
        assert!(
            word.starts_with("ok（") && word.contains(NEW) && word.contains(MISMATCH_CLAUSE),
            "终态词在前、投递号与明示语在后: {word}"
        );
    }

    /// 回执明示的应用单点（评审 I6）：只有 `NewSession` 且**非 Failed** 才追加
    /// （Failed 时正文未必送达——说「消息已进入新建会话」是超出证据的结论）；
    /// 原成因保留在前，明示语在后。
    #[test]
    fn apply_targeting_note_only_for_new_session_success() {
        const CARD: &str = "session-bbbb";
        const NEW: &str = "3fb0c1d2-aaaa-4bbb-8ccc-dddddddddddd";
        let mut ok = Receipt::ok(CARD, 12);
        ok.reason = Some("回合完成".to_string());
        assert!(apply_targeting_note(
            &mut ok,
            &Targeting::NewSession {
                session_id: NEW.to_string()
            },
            CARD
        ));
        let reason = ok.reason.clone().unwrap_or_default();
        assert!(
            reason.starts_with("回合完成；") && reason.contains(MISMATCH_CLAUSE),
            "原成因在前、明示语在后: {reason}"
        );
        // resume 命中 / 桌面占用（零字节投递）⇒ 不标注
        let mut resumed = Receipt::ok(CARD, 12);
        assert!(!apply_targeting_note(
            &mut resumed,
            &Targeting::Resumed {
                session_id: CARD.to_string()
            },
            CARD
        ));
        assert_eq!(resumed.reason, None);
        let mut refused = Receipt::failed(Stage::Refused, "桌面写者占用");
        assert!(!apply_targeting_note(
            &mut refused,
            &Targeting::RefusedDesktopWriter {
                session_id: CARD.to_string(),
                lock_message: "lock".to_string()
            },
            CARD
        ));
        // 失败态即便走 NewSession 也不标注（正文未必送达——不超证据）
        let mut failed = Receipt::failed(Stage::ChannelError, "回合失败（协议错误 -32000）");
        assert!(!apply_targeting_note(
            &mut failed,
            &Targeting::NewSession {
                session_id: NEW.to_string()
            },
            CARD
        ));
        assert_eq!(
            failed.reason.as_deref(),
            Some("回合失败（协议错误 -32000）")
        );
    }

    /// list/resume 形态会话号（`session-<uuid>` 前缀——探测 §2.2/§2.4 二象性之一）
    const SID: &str = "session-9f14a8c5-1111-4222-8333-444455556666";
    /// `session/new` 返回的裸 `<uuid>` 形态（无前缀——原样回传绝不归一化）
    const NEW_SID: &str = "3fb0c1d2-aaaa-4bbb-8ccc-dddddddddddd";
    /// 桌面写者锁错误原文（探测 §2.3 活体形态：-32603 + data.details 内嵌锁串）
    const LOCK_MSG: &str =
        "Internal error, data.details: \"session-9f14… is already owned by an active write handle\"";

    /// cwd 夹具（绝对路径——探测 §2.2：相对路径被 -32602 拒；跨平台取当前平台形态）
    fn cwd_fixture() -> String {
        if cfg!(windows) {
            "E:\\LLMproject\\probe".to_string()
        } else {
            "/tmp/probe".to_string()
        }
    }

    // ---- 帧构造夹具（标准 ACP 方言原文；json! 宏负责转义）----

    fn resp(id: u64, result: &str) -> String {
        format!(
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": serde_json::from_str::<serde_json::Value>(result).unwrap()})
        )
    }

    fn resp_err(id: u64, code: i64, message: &str) -> String {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            .to_string()
    }

    fn note(method: &str, params: serde_json::Value) -> String {
        serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
    }

    fn server_req(id: u64, method: &str, params: serde_json::Value) -> String {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string()
    }

    /// initialize 成功回包（探测 §2.1 活体形状；image 位动态——此处 false 仅为夹具）
    fn init_result() -> String {
        serde_json::json!({
            "protocolVersion": 1,
            "agentInfo": {"name": "deepseek-harness-acp", "version": "0.0.1"},
            "agentCapabilities": {
                "mcpCapabilities": {"http": true},
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
                "sessionCapabilities": {"close": {}, "list": {}, "resume": {}}
            },
            "authMethods": []
        })
        .to_string()
    }

    fn init_ok() -> String {
        resp(1, &init_result())
    }

    /// authenticate 恒成功（源码直读——空结果形态）
    fn auth_ok() -> String {
        resp(2, "{}")
    }

    fn list_result(sessions: serde_json::Value, next_cursor: Option<&str>) -> String {
        let mut obj = serde_json::Map::new();
        obj.insert("sessions".to_string(), sessions);
        if let Some(c) = next_cursor {
            obj.insert("nextCursor".to_string(), serde_json::json!(c));
        }
        format!("{}", serde_json::Value::Object(obj))
    }

    /// 含目标会话的单页 list 结果
    fn list_with_target() -> String {
        list_result(
            serde_json::json!([{"sessionId": SID, "cwd": cwd_fixture()}]),
            None,
        )
    }

    /// resume 成功回显（探测 §2.3：configOptions 的 currentValue 为 JSON 数组字符串形态）
    fn resume_ok() -> String {
        resp(
            4,
            r#"{"configOptions":[{"name":"model","kind":"select","currentValue":"[\"deepseek-official\",\"deepseek-v4-flash\"]"},{"name":"reasoning_effort","kind":"select","currentValue":"\"low\""}]}"#,
        )
    }

    fn stop_reason_resp(id: u64, reason: &str) -> String {
        // stopReason 值域未验（M25 兜底）——夹具值不代表枚举承诺
        resp(id, &format!("{{\"stopReason\":\"{reason}\"}}"))
    }

    fn update_note(update: serde_json::Value) -> String {
        note(
            "session/update",
            serde_json::json!({"sessionId": SID, "update": update}),
        )
    }

    fn msg_chunk_flat(text: &str) -> String {
        // 平铺形态（嵌套层级未活体取证——双路防御的兜底路夹具）
        note(
            "session/update",
            serde_json::json!({"sessionId": SID, "agent_message_chunk": {"messageId": "m1", "content": {"type": "text", "text": text}}}),
        )
    }

    // ---- MockIo / 记录型 spawn 缝（照 codex_app.rs:1029 先例；零进程零网络）----

    #[derive(Default, Debug)]
    struct SpawnLog {
        specs: Vec<SpawnSpec>,
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

    impl AcpIo for MockIo {
        fn send(&mut self, line: &str) -> Result<(), String> {
            self.sent.push(line.to_string());
            Ok(())
        }

        fn recv(&mut self) -> Option<Result<String, String>> {
            // "@ERR:<msg>" 前缀 = IO 错误/超时形态（spec m5 测试基建）；其余 = 正常行
            match self.script.pop_front()? {
                item if item.starts_with("@ERR:") => Some(Err(item[5..].to_string())),
                item => Some(Ok(item)),
            }
        }

        fn finish(&mut self) {
            self.finished = true;
            self.commit();
        }
    }

    impl Drop for MockIo {
        fn drop(&mut self) {
            self.commit();
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
            }) as Box<dyn AcpIo>)
        });
        (seam, log)
    }

    fn spec() -> SpawnSpec {
        SpawnSpec {
            argv: acp_argv("C:/probe/node.exe", "C:/cache/bin.js"),
            env: acp_env("C:/Users/x/.dsh"),
        }
    }

    fn client_of(seam: SpawnSeam) -> DshAcpClient {
        DshAcpClient::new(seam, spec())
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

    /// T0.3 spawn 钉值：argv 形状 / env 两键 / 缓存布局 / 幂等决策 / --patch 红线不在场
    #[test]
    fn spawn_argv_matches_probed_definition() {
        let node =
            "C:/Users/x/.dsh/dsh-runtimes/dsh-primary-runtime/dependencies/node/bin/node.exe";
        let bin = "C:/tuvis-cache/dsh-asar/0.2.0-rc.2/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js";
        let argv = acp_argv(node, bin);
        assert_eq!(
            argv,
            vec![
                node.to_string(),
                bin.to_string(),
                "--profile".to_string(),
                "acp".to_string()
            ],
            "T0.3 钉值：runtime node + 解包 bin.js + --profile acp"
        );
        assert!(
            !argv.iter().any(|a| a == "--patch"),
            "--patch 不在钉值 argv——acp config 覆盖是整对象替换（探测 §1.5 实事故），\
             本客户端不覆写 config"
        );
        // env 两键：DSH_HOME 透传 + 关遥测
        assert_eq!(
            acp_env("C:/Users/x/.dsh"),
            vec![
                ("DSH_HOME".to_string(), "C:/Users/x/.dsh".to_string()),
                ("DSH_TELEMETRY_DISABLED".to_string(), "1".to_string())
            ]
        );
        // 受管缓存布局（单点 EXTRACT_SUBTREE_REL）+ 版本净化
        assert_eq!(
            bin_js_path("C:/tuvis-cache/dsh-asar", "0.2.0-rc.2"),
            "C:/tuvis-cache/dsh-asar/0.2.0-rc.2/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js"
        );
        assert_eq!(
            sanitize_version("0.2.0-rc.2"),
            "0.2.0-rc.2",
            "合法版本串原样保留（缓存键 = desktopVersion）"
        );
        assert_eq!(
            sanitize_version("a/b\\c:d"),
            "a_b_c_d",
            "路径危险字符全替 _（防注入）"
        );
        assert_eq!(
            sanitize_version("."),
            "unknown",
            "spec m1：缓存子目录名不得是当前目录 `.`（越界写根目录）"
        );
        assert_eq!(
            sanitize_version(".."),
            "unknown",
            "spec m1：缓存子目录名不得是父目录 `..`（路径穿越）"
        );
        assert_eq!(sanitize_version(""), "unknown");
        assert_eq!(
            extract_marker_path("cache", "0.2.0"),
            format!("cache/0.2.0/{EXTRACT_MARKER_FILE}")
        );
        // 幂等决策：标记逐字相等 → Skip；漂移/首启 → Extract
        let fp = extract_fingerprint(402_653_184, 1_760_230_000);
        assert_eq!(fp, "size=402653184,mtime=1760230000");
        assert_eq!(
            extract_decision(Some(&fp), &fp),
            ExtractDecision::Skip,
            "digest 相等 → 跳过重解（幂等）"
        );
        assert_eq!(
            extract_decision(Some(&fp), &extract_fingerprint(1, 2)),
            ExtractDecision::Extract,
            "asar 升级（指纹漂移）→ 重解"
        );
        assert_eq!(
            extract_decision(None, &fp),
            ExtractDecision::Extract,
            "首启（无标记）→ 解包"
        );
    }

    /// 三态分派主断言：①非活跃 → resume+prompt 精确靶向；②活跃（桌面写者）→ 如实拒；
    /// ③无既有会话 → new（mcpServers:[]）+ prompt
    #[test]
    fn three_state_dispatch_inactive_resumes_active_refuses_absent_news() {
        let cwd = cwd_fixture();
        // ① 非活跃 → resume + prompt + close（帧序：init/auth/list/resume/prompt/close）
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, &list_with_target()),
            resume_ok(),
            stop_reason_resp(5, "end_turn"),
            resp(6, "{}"),
        ]]);
        let r = client_of(seam).send(SID, &cwd, "你好").unwrap();
        assert_eq!(
            r.targeting,
            Targeting::Resumed {
                session_id: SID.to_string()
            }
        );
        assert_eq!(r.receipt.status, ReceiptStatus::Ok);
        assert_eq!(r.receipt.session_id, SID);
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
        let resume_frame = sent_json(&log, 3);
        assert_eq!(resume_frame["method"], "session/resume");
        assert_eq!(resume_frame["params"]["sessionId"], SID);
        assert_eq!(
            resume_frame["params"]["cwd"], cwd,
            "resume cwd 必填且与持久化一致"
        );
        let prompt_frame = sent_json(&log, 4);
        assert_eq!(prompt_frame["method"], "session/prompt");
        assert_eq!(prompt_frame["params"]["sessionId"], SID);
        assert_eq!(prompt_frame["params"]["prompt"][0]["type"], "text");
        assert_eq!(prompt_frame["params"]["prompt"][0]["text"], "你好");
        let close_frame = sent_json(&log, 5);
        assert_eq!(
            close_frame["method"], "session/close",
            "每会话必收尾释放写锁"
        );
        assert_eq!(close_frame["params"]["sessionId"], SID);
        assert!(log.lock().unwrap().instances[0].finished);
        // ② 活跃（桌面写者占用）→ 如实拒：零 prompt 帧、零 close 帧（未持写者）
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, &list_with_target()),
            resp_err(4, INTERNAL_ERROR_CODE, LOCK_MSG),
        ]]);
        let r = client_of(seam).send(SID, &cwd, "你好").unwrap();
        assert!(matches!(
            &r.targeting,
            Targeting::RefusedDesktopWriter { session_id, lock_message }
                if session_id == SID && lock_message.contains(DESKTOP_WRITER_LOCK_MARKER)
        ));
        assert_eq!(r.receipt.status, ReceiptStatus::Failed);
        assert_eq!(
            serde_json::to_value(r.receipt.stage.unwrap()).unwrap(),
            serde_json::json!("refused"),
            "会话被占用 = 零字节投递的 Refused 面"
        );
        assert!(r.stop_reason.is_none());
        let frames = sent_frames(&log);
        assert!(
            !frames.iter().any(|f| f.contains("session/prompt")),
            "活跃会话绝不投 prompt"
        );
        assert!(
            !frames.iter().any(|f| f.contains("session/close")),
            "未持有写者不发 close"
        );
        // ③ 无既有会话 → new（mcpServers 必填 []）+ prompt + close；sessionId 二象性原样回传
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, &list_result(serde_json::json!([]), None)),
            resp(
                4,
                &format!("{{\"sessionId\":\"{NEW_SID}\",\"configOptions\":[]}}"),
            ),
            stop_reason_resp(5, "end_turn"),
            resp(6, "{}"),
        ]]);
        let r = client_of(seam).send(SID, &cwd, "你好").unwrap();
        assert_eq!(
            r.targeting,
            Targeting::NewSession {
                session_id: NEW_SID.to_string()
            }
        );
        assert_eq!(r.receipt.session_id, NEW_SID);
        let new_frame = sent_json(&log, 3);
        assert_eq!(new_frame["method"], "session/new");
        assert_eq!(
            new_frame["params"]["mcpServers"],
            serde_json::json!([]),
            "mcpServers 必填（比标准 ACP 严——探测 §2.4）"
        );
        assert_eq!(new_frame["params"]["cwd"], cwd);
        let prompt_frame = sent_json(&log, 4);
        assert_eq!(
            prompt_frame["params"]["sessionId"], NEW_SID,
            "new 返回裸 <uuid> 原样回传——绝不归一化加 session- 前缀"
        );
        let close_frame = sent_json(&log, 5);
        assert_eq!(close_frame["params"]["sessionId"], NEW_SID);
    }

    /// 如实拒文案点名桌面写者；错误变体 Display/wire_word；锁判据特异性
    /// （-32603 需特征串同现；provider 特征单列；裸 -32603 不当锁）
    #[test]
    fn active_session_refusal_copy_names_desktop_writer() {
        // 拒文案逐字钉值（设计 §6.1）
        assert_eq!(ACTIVE_SESSION_REFUSAL, "会话正被桌面使用，请关闭后再投");
        // send 层：reason 即拒文案
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, &list_with_target()),
            resp_err(4, INTERNAL_ERROR_CODE, LOCK_MSG),
        ]]);
        let r = client_of(seam).send(SID, &cwd_fixture(), "hi").unwrap();
        assert_eq!(
            r.receipt.reason.as_deref(),
            Some(ACTIVE_SESSION_REFUSAL),
            "拒收据 reason = 如实拒文案（不含锁技术原文——那是 targeting.lock_message 的事）"
        );
        // 错误变体面：Display 点名桌面写者；wire_word 钉值
        let e = DshAcpError::DesktopWriterActive {
            lock_message: LOCK_MSG.to_string(),
        };
        assert_eq!(e.wire_word(), DESKTOP_WRITER_ACTIVE);
        let shown = e.to_string();
        assert!(
            shown.contains("桌面") && shown.contains(ACTIVE_SESSION_REFUSAL),
            "Display 面向用户点名桌面写者：{shown}"
        );
        // 锁判据特异性（增量约束 1）：-32603 + 特征串同现才定性
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp_err(3, INTERNAL_ERROR_CODE, LOCK_MSG),
        ]]);
        let mut s = client_of(seam).session().unwrap();
        assert!(matches!(
            s.resume_session(SID, &cwd_fixture()),
            Err(DshAcpError::DesktopWriterActive { .. })
        ));
        s.exit();
        // 同码不同串：余额不足（回合失败面）→ Rpc，绝不冒充桌面占用
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp_err(
                3,
                INTERNAL_ERROR_CODE,
                "turn failed: Insufficient Balance (request_id: r1)",
            ),
        ]]);
        let mut s = client_of(seam).session().unwrap();
        let err = s.resume_session(SID, &cwd_fixture()).unwrap_err();
        assert!(matches!(err, DshAcpError::Rpc { code, .. } if code == INTERNAL_ERROR_CODE));
        assert_eq!(err.wire_word(), "rpc_error");
        s.exit();
        // provider 不可用 → 单列变体（事后判——list 无 provider 字段）
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp_err(
                3,
                INTERNAL_ERROR_CODE,
                "no adapter registered for provider \"ollama\"",
            ),
        ]]);
        let mut s = client_of(seam).session().unwrap();
        let err = s.resume_session(SID, &cwd_fixture()).unwrap_err();
        assert!(matches!(err, DshAcpError::ProviderUnavailable { .. }));
        assert_eq!(err.wire_word(), PROVIDER_UNAVAILABLE);
        s.exit();
        // 裸 -32600（cwd 非绝对等参数面）→ 原样透传
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp_err(3, -32602, "cwd must be an absolute path"),
        ]]);
        let mut s = client_of(seam).session().unwrap();
        assert!(matches!(
            s.resume_session(SID, &cwd_fixture()),
            Err(DshAcpError::Rpc { code: -32602, .. })
        ));
        s.exit();
    }

    /// list 的 cwd 筛选参数逐字进帧 + keyset-cursor 翻页跟随；相对 cwd 投递前拒零 spawn
    #[test]
    fn session_list_filters_by_cwd() {
        let cwd = cwd_fixture();
        // 两页翻页：第一页带 cwd 无 cursor；第二页带 cursor + cwd
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(
                3,
                &list_result(
                    serde_json::json!([{"sessionId": "session-a", "cwd": cwd}]),
                    Some("CUR-1"),
                ),
            ),
            resp(
                4,
                &list_result(
                    serde_json::json!([{"sessionId": "session-b", "cwd": cwd}]),
                    None,
                ),
            ),
        ]]);
        let mut s = client_of(seam).session().unwrap();
        let entries = s.list_sessions(&cwd).unwrap();
        s.exit();
        assert_eq!(entries.len(), 2, "keyset cursor 翻到尽头——两页累积");
        assert_eq!(entries[0].session_id, "session-a");
        assert_eq!(entries[1].session_id, "session-b");
        assert_eq!(entries[0].cwd, cwd);
        let page1 = sent_json(&log, 2);
        assert_eq!(page1["method"], "session/list");
        assert_eq!(
            page1["params"]["cwd"], cwd,
            "cwd 筛选参数逐字进帧（绝对路径）"
        );
        assert!(
            page1["params"].get("cursor").is_none(),
            "首页无 cursor 参数"
        );
        let page2 = sent_json(&log, 3);
        assert_eq!(
            page2["params"]["cursor"], "CUR-1",
            "次页带前页 nextCursor（keyset 翻页）"
        );
        assert_eq!(page2["params"]["cwd"], cwd, "翻页页也带 cwd 筛选");
        // 相对 cwd → 投递前拒（零 spawn——MAM 侧先于服务端 -32602 拒，零协议副作用）
        let (seam, log) = mock_spawn(vec![]);
        assert!(matches!(
            client_of(seam).send(SID, "relative/path", "x"),
            Err(DshAcpError::InvalidTarget(_))
        ));
        assert!(
            log.lock().unwrap().specs.is_empty(),
            "相对 cwd 在 spawn 前拒——零进程零协议副作用"
        );
    }

    /// prompt 回执归一：update 六变体流（源码形状）累积 → 末条 assistant 摘要 + 时长 +
    /// stopReason 透传；usage_update 是上下文占用不冒充输出 token；嵌套双路防御
    #[test]
    fn prompt_receipt_from_session_update_stream() {
        let cwd = cwd_fixture();
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, &list_with_target()),
            resume_ok(),
            // 六变体（源码形状，T0.4 §2.5——余额阻断未活体取证，M25 转正）
            update_note(
                serde_json::json!({"agent_thought_chunk": {"messageId": "th1", "content": {"type": "text", "text": "思考中"}}}),
            ),
            update_note(
                serde_json::json!({"agent_message_chunk": {"messageId": "m1", "content": {"type": "text", "text": "你"}}}),
            ),
            msg_chunk_flat("好"), // 平铺形态兜底路（嵌套层级未取证）
            update_note(
                serde_json::json!({"tool_call": {"toolCallId": "t1", "title": "shell", "kind": "other", "status": "in_progress", "rawInput": {}}}),
            ),
            update_note(
                serde_json::json!({"tool_call_update": {"toolCallId": "t1", "status": "completed", "content": []}}),
            ),
            update_note(serde_json::json!({"usage_update": {"used": 1200, "size": 128000}})),
            update_note(serde_json::json!({"config_option_update": {"configOptions": []}})),
            update_note(serde_json::json!({"future_variant": {}})), // 未知变体只计数
            stop_reason_resp(5, "end_turn"),
            resp(6, "{}"),
        ]]);
        let r = client_of(seam).send(SID, &cwd, "问题").unwrap();
        assert_eq!(r.receipt.status, ReceiptStatus::Ok);
        // 末条 assistant：同 messageId 的 chunk 追加（"你"+"好"——含平铺路的第二帧）
        assert_eq!(r.updates.last_message.as_deref(), Some("你好"));
        assert_eq!(
            r.receipt.last_assistant.as_deref(),
            Some("你好"),
            "末条 assistant 摘要（turn.rs 底座单点截断口径）"
        );
        // 计数面
        assert_eq!(r.updates.thought_chunks, 1);
        assert_eq!(r.updates.message_chunks, 2);
        assert_eq!(r.updates.tool_calls, 1);
        assert_eq!(r.updates.tool_call_updates, 1);
        assert_eq!(r.updates.usage_updates, 1);
        assert_eq!(r.updates.config_option_updates, 1);
        assert_eq!(r.updates.unknown_updates, 1, "未知变体只计数不解释");
        // usage_update = 上下文窗口占用——透出但不冒充输出 token
        assert_eq!(r.updates.context_used, Some(1200));
        assert_eq!(r.updates.context_size, Some(128000));
        assert!(
            r.receipt.tokens.is_none(),
            "used/size 是上下文占用不是输出 token——Receipt.tokens 恒 None 不冒充"
        );
        // stopReason 在场透传（值域未验——不硬编码枚举）
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
        // 回合失败面（-32603 + 服务端原文）→ 回执 ChannelError 携原文，不谎报成功
        let (seam, _) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, &list_with_target()),
            resume_ok(),
            resp_err(
                5,
                INTERNAL_ERROR_CODE,
                "turn failed: Insufficient Balance (request_id: r1)",
            ),
            resp(6, "{}"),
        ]]);
        let r = client_of(seam).send(SID, &cwd, "问题").unwrap();
        assert_eq!(r.receipt.status, ReceiptStatus::Failed);
        let reason = r.receipt.reason.unwrap();
        assert!(
            reason.contains("Insufficient Balance"),
            "服务端原文透传：{reason}"
        );
        assert!(r.stop_reason.is_none());
    }

    /// cancel 按 T0.4 定案 = notification 形态回执：无 id 帧 + 如实注记（无回执可核对）+
    /// 不谎报已确认；空 sessionId 投递前拒
    #[test]
    fn cancel_receipt_or_honest_unavailable() {
        let (seam, log) = mock_spawn(vec![vec![init_ok(), auth_ok()]]);
        let mut s = client_of(seam).session().unwrap();
        let notice = s.cancel(SID).unwrap();
        s.exit();
        assert!(
            notice.sent,
            "notification 已写出（sent 只代表已写出，不代表已确认——结算以回合响应为准）"
        );
        assert_eq!(notice.session_id, SID);
        assert_eq!(
            notice.note, CANCEL_NOTE,
            "如实注记钉值（M25 验证前不谎报取消效果）"
        );
        assert!(notice.note.contains("notification"));
        // 帧形态：无 id（notification）；params 带 sessionId
        let frame = sent_json(&log, 2);
        assert_eq!(frame["method"], "session/cancel");
        assert_eq!(frame["params"]["sessionId"], SID);
        assert!(
            frame.get("id").is_none(),
            "cancel 是 notification——绝无 id（也不会等响应帧，无帧可冒充回执）"
        );
        // 空 sessionId → 投递前拒
        let (seam, _) = mock_spawn(vec![vec![init_ok(), auth_ok()]]);
        let mut s = client_of(seam).session().unwrap();
        assert!(matches!(s.cancel("  "), Err(DshAcpError::InvalidTarget(_))));
        s.exit();
    }

    /// request_permission 透传：收集 + 展示文案；**绝不回发 outcome 帧**（代答列 P2）；
    /// 权限请求不阻塞 prompt 收口
    #[test]
    fn request_permission_passthrough_display_only_never_autoanswers() {
        let cwd = cwd_fixture();
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, &list_with_target()),
            resume_ok(),
            server_req(
                900,
                "session/request_permission",
                serde_json::json!({
                    "sessionId": SID,
                    "toolCall": {"toolCallId": "tc-9"},
                    "options": [
                        {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                        {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"}
                    ]
                }),
            ),
            update_note(
                serde_json::json!({"agent_message_chunk": {"messageId": "m1", "content": {"type": "text", "text": "完成"}}}),
            ),
            stop_reason_resp(5, "end_turn"),
            resp(6, "{}"),
        ]]);
        let r = client_of(seam).send(SID, &cwd, "做点事").unwrap();
        // prompt 收口不受权限请求阻塞（服务端 drain 在途 update 后结算——源码形状）
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
        // Important 1：权限请求经 send() 主入口透传到回执（Task 10 接线面可达——
        // 不再需要会话级复验 hack）；只收集展示不代答
        assert_eq!(
            r.permission_requests.len(),
            1,
            "权限请求被收集并随回执透出（等待窗内）"
        );
        let pr = &r.permission_requests[0];
        assert_eq!(pr.id, 900);
        assert_eq!(pr.session_id.as_deref(), Some(SID));
        assert_eq!(pr.tool_call_id.as_deref(), Some("tc-9"));
        assert_eq!(pr.options.len(), 2);
        assert_eq!(pr.options[0].option_id, "allow-once");
        assert_eq!(pr.options[0].name, "Allow once");
        assert_eq!(pr.options[0].kind, "allow_once");
        assert_eq!(pr.options[1].option_id, "reject-once");
        let display = pr.display();
        assert!(
            display.contains("dsh 请求权限") && display.contains("tc-9"),
            "上屏文案点名工具调用：{display}"
        );
        // **绝不代答**（Minor 8：死断言修正为独立断言）：所有已发帧是请求/通知——
        // 绝无 id=900 的响应帧、绝无 result 键、绝无 outcome 载荷
        for f in sent_frames(&log) {
            let v: serde_json::Value = serde_json::from_str(&f).unwrap();
            assert!(
                v.get("id").and_then(|i| i.as_u64()) != Some(900),
                "绝不回发 id=900 的响应帧（代答列 P2）：{f}"
            );
            assert!(
                v.get("result").is_none(),
                "已发帧都是请求/通知——不含 result 键（响应面只属于代答）：{f}"
            );
            assert!(
                v.to_string().find("\"outcome\"").is_none(),
                "绝无 outcome 载荷（allow-once/reject-once 应答是 P2 代答面）：{f}"
            );
        }
    }

    /// Minor 9：list 分页护栏——预算注入缩短（3 页）+ 恒带 nextCursor 的循环桩 →
    /// 预算耗尽仍见 cursor = Channel 如实报（不静默截断、不发第 4 页）
    #[test]
    fn list_pagination_guard_stops_at_budget() {
        fn page(id: u64, cursor: &str) -> String {
            resp(
                id,
                &list_result(
                    serde_json::json!([{"sessionId": "session-x", "cwd": cwd_fixture()}]),
                    Some(cursor),
                ),
            )
        }
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            page(3, "CUR-1"),
            page(4, "CUR-2"),
            page(5, "CUR-3"),
        ]]);
        let mut s = client_of(seam).session().unwrap();
        let err = s.list_sessions_with_budget(&cwd_fixture(), 3).unwrap_err();
        s.exit();
        assert!(
            matches!(&err, DshAcpError::Channel(m) if m.contains("护栏")),
            "护栏触发按通道错误面如实报：{err:?}"
        );
        let sent = sent_frames(&log);
        assert_eq!(
            sent.len(),
            5,
            "预算 3 页 = initialize + authenticate + 3 页 list，第 4 页绝不发出"
        );
    }

    /// spec m5：prompt 通道级失败（读超时/EOF）→ 跳过 session/close 直接优雅退出——
    /// prompt 已阻塞过一个 300s 读预算，close 再等响应 = 再赌一个读预算
    #[test]
    fn prompt_channel_failure_skips_close_and_exits() {
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, &list_with_target()),
            resume_ok(),
            "@ERR:dsh-acp 300000ms 无输出（读取超时）".to_string(),
        ]]);
        let err = client_of(seam).send(SID, &cwd_fixture(), "hi").unwrap_err();
        assert!(
            matches!(&err, DshAcpError::Channel(m) if m.contains("读取超时")),
            "读超时按通道错误面透传：{err:?}"
        );
        let frames = sent_frames(&log);
        assert!(
            !frames.iter().any(|f| f.contains("session/close")),
            "通道级失败跳过 close——进程退出即弃连接（close 再等响应 = 再赌一个读预算）"
        );
        assert!(
            log.lock().unwrap().instances[0].finished,
            "仍优雅退出（关 stdin）"
        );
    }

    /// initialize/authenticate 失败一律通道错误面（EOF / 错误帧 / spawn 失败）
    #[test]
    fn initialize_failure_surfaces_channel_error() {
        // 形态 a：EOF——spawn 成功但零输出即关
        let (seam, _) = mock_spawn(vec![vec![]]);
        let err = DshAcpSession::start(&seam, &spec())
            .err()
            .expect("EOF 无响应必须失败");
        assert!(
            matches!(&err, DshAcpError::Channel(m) if m.contains("EOF")),
            "EOF 无响应 → Channel：{err:?}"
        );
        // 形态 b：错误帧应答 initialize
        let (seam, _) = mock_spawn(vec![vec![resp_err(1, -32000, "Server error")]]);
        let err = DshAcpSession::start(&seam, &spec())
            .err()
            .expect("错误帧应答 initialize 必须失败");
        assert!(
            matches!(&err, DshAcpError::Channel(m) if m.contains("initialize")),
            "握手失败一律通道面：{err:?}"
        );
        // 形态 c：authenticate 失败（源码恒成功——失败即通道异常面）
        let (seam, _) = mock_spawn(vec![vec![init_ok(), resp_err(2, -32000, "auth boom")]]);
        let err = DshAcpSession::start(&seam, &spec())
            .err()
            .expect("authenticate 失败必须失败");
        assert!(
            matches!(&err, DshAcpError::Channel(m) if m.contains("authenticate")),
            "authenticate 失败归通道面（不是方法语义）：{err:?}"
        );
        // 形态 d：spawn 失败本身（脚本耗尽）
        let (seam, _) = mock_spawn(vec![]);
        let err = DshAcpSession::start(&seam, &spec())
            .err()
            .expect("spawn 失败必须失败");
        assert!(matches!(err, DshAcpError::Channel(_)), "{err:?}");
    }

    /// 短命生命周期：每动作一枚实例（initialize → authenticate → … → close → finish）
    #[test]
    fn per_action_session_spawns_and_exits() {
        fn send_script() -> Vec<String> {
            vec![
                init_ok(),
                auth_ok(),
                resp(3, &list_with_target()),
                resume_ok(),
                stop_reason_resp(5, "end_turn"),
                resp(6, "{}"),
            ]
        }
        let (seam, log) = mock_spawn(vec![send_script(), send_script()]);
        let client = client_of(seam);
        client.send(SID, &cwd_fixture(), "一").unwrap();
        client.send(SID, &cwd_fixture(), "二").unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.specs.len(), 2, "两个动作 = 两枚实例（无常驻池）");
        assert_eq!(log.instances.len(), 2);
        for inst in &log.instances {
            assert!(
                inst.finished,
                "每枚实例都被显式收尾（优雅退出——持写者绝不硬杀）"
            );
            let first: serde_json::Value = serde_json::from_str(&inst.sent[0]).unwrap();
            assert_eq!(first["method"], "initialize", "每实例第一帧 = initialize");
            let second: serde_json::Value = serde_json::from_str(&inst.sent[1]).unwrap();
            assert_eq!(
                second["method"], "authenticate",
                "第二帧 = authenticate（显式调用——恒成功免鉴权）"
            );
            let last: serde_json::Value = serde_json::from_str(inst.sent.last().unwrap()).unwrap();
            assert_eq!(
                last["method"], "session/close",
                "末帧 = session/close（释放写锁后再退出）"
            );
        }
    }

    // ---- 附加钉值 ----

    /// set_config_option 的 value 必须是 JSON 字符串形态（类型缝——不接线，钉 wire 形状）
    #[test]
    fn set_config_option_value_is_json_string_form() {
        let (seam, log) = mock_spawn(vec![vec![
            init_ok(),
            auth_ok(),
            resp(3, r#"{"configOptions":[]}"#),
        ]]);
        let mut s = client_of(seam).session().unwrap();
        let value = r#"["deepseek-official","deepseek-v4-flash"]"#;
        s.set_config_option(SID, "model", value).unwrap();
        s.exit();
        let frame = sent_json(&log, 2);
        assert_eq!(frame["method"], "session/set_config_option");
        assert_eq!(frame["params"]["sessionId"], SID);
        assert_eq!(frame["params"]["name"], "model");
        assert!(
            frame["params"]["value"].is_string(),
            "value 必须是字符串形态的 JSON（传数组会被 schema 拒——探测 §2.4）"
        );
        assert_eq!(frame["params"]["value"], value);
    }

    /// 范围边界登记（设计 §6.1 不做清单——客户端不实现，路由层如实 unsupported）
    #[test]
    fn unsupported_surface_registered() {
        for token in ["mode", "load", "fork", "delete", "sse", "19387", "webhook"] {
            assert!(
                UNSUPPORTED_SURFACE.contains(token),
                "不做清单须登记 {token}：{UNSUPPORTED_SURFACE}"
            );
        }
    }

    /// 宽松方言（判序照 codex_app.rs:110 先例）：无 `jsonrpc` 字段照解析（在场不拒）；
    /// 非 JSON 归未知；服务端请求 params 透传
    #[test]
    fn frame_parse_is_lenient_about_jsonrpc_field() {
        assert_eq!(
            parse_frame(r#"{"id":1,"result":{"protocolVersion":1}}"#),
            WireFrame::Response {
                id: 1,
                payload: FramePayload::Ok(serde_json::json!({"protocolVersion": 1}))
            },
            "无 jsonrpc 字段照解析（不按 2.0 严格校验）"
        );
        assert!(matches!(
            parse_frame(
                r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found"}}"#
            ),
            WireFrame::Response {
                id: 2,
                payload: FramePayload::Err(FrameError { code: -32601, .. })
            }
        ));
        let sr = parse_frame(
            r#"{"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"options":[]}}"#,
        );
        assert!(matches!(
            &sr,
            WireFrame::ServerRequest { id: 900, method, .. }
                if method == "session/request_permission"
        ));
        let n = parse_frame(r#"{"method":"session/update","params":{"sessionId":"s"}}"#);
        assert!(matches!(
            &n,
            WireFrame::Notification { method, .. } if method == "session/update"
        ));
        assert_eq!(parse_frame("noise not json"), WireFrame::Unknown);
        assert_eq!(parse_frame(""), WireFrame::Unknown);
    }
}

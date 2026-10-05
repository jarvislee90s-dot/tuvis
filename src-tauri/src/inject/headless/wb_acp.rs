//! H9 WorkBuddy ACP 通道（Task 11）：**HTTP 型**无头通道——**不 spawn 进程**，走 WorkBuddy
//! 宿主运行时的 ACP over HTTP（探测定案：Mac 主证，`research/refs/phase2-消息注入/
//! 2026-10-04-app-injection-probe-report-mac.md` 末轮；Windows 5.7.3 端点启用条件 = 风险 16）。
//!
//! # 协议链（Mac 实测 wire 为准）
//! `POST <endpoint>/api/v1/acp/connect`（**免鉴权**）→ `{connectionId, sessionToken}` →
//! 逐请求 `POST <endpoint>/api/v1/acp`（头 `acp-connection-id` / `acp-session-token` /
//! `Accept: application/json, text/event-stream`——**Accept 缺一即 `-32000 Not Acceptable`**，
//! 见探测定案逐字）→ `initialize`（能力协商）→ **目标语义二分**（活跃 = `session/load` +
//! `session/prompt`；新建 = `session/new` + `session/prompt`）→ SSE 事件归一（`agentPhase` /
//! `session_update`）→ 回执。
//!
//! # 诚实边界（**落码于此，勿在别处另说一套**）
//! - **已结束会话静默挂**（探测定案风险 15）：`session/prompt` 只对**新建/活跃**会话生效；
//!   对 `session/load` 载入的**已结束**会话（末回合 `stopReason=end_turn`）会「200 + 心跳但
//!   回合不推进」。故 [`PHASE_DEADLINE_MS`] 内**未见到 `agentPhase`** 即判「已结束需复活」
//!   （[`PromptVerdict::RevivalNeeded`]）——**绝不静默挂、绝不冒充成功**。注意：`session/load`
//!   回放里出现 `session_end(end_turn)` **不是**「该会话不可投递」的判据（任何跑完过一回合的
//!   会话回放都有它），它只作**诊断证据**写进复活回执的原因里（见 [`LoadOutcome`]）。
//! - **端点发现平台差异**：macOS/旧版 = 心跳 `~/.workbuddy/sessions/<pid>.json` 的 `endpoint`
//!   字段（每会话异端口、**按需轮询**——心跳按需生成，查空 ≠ 形态不存在）；Windows 5.7.3 =
//!   无交互心跳且无 per-session serve ⇒ **心跳缺席时直接如实拒绝**（[`ENDPOINT_UNAVAILABLE_REASON`]，
//!   不盲扫），启用了远程控制才走**端口指纹**路（`Get-NetTCPConnection` 按 WB pid 过滤 →
//!   `/` 标题指纹 + `/health` `{"status":"UP"}` **双确认**）。
//! - ACP 会话**不进 `workbuddy.db`**，只落 `~/.workbuddy/projects/<munged-cwd>/` 转写 ⇒
//!   读链路补扫归 `monitor::workbuddy_parser`（Task 11 同批）；本模块只做**落盘佐证**
//!   （[`corroborate`]，二次确认，不作唯一证据）。
//! - Permission Mode 是协议级四档（default/acceptEdits/plan/auto）**只读**——本模块只把它
//!   归一到 [`PromptOutcome::permission_mode`]，**没有展示面也没有切换面**（切换留三期 F3.1；
//!   展示随审批卡归 C4/Task 13）。
//! - 取消：本通道**不武装取消靶子**（槽位用 [`super::turn::TurnSlot::placeholder`]）——ACP 的
//!   `session/cancel` 通知**本批未接线**，故取消端点如实报「未送达」（不谎报已取消）；理由
//!   另见编排函数文档（武装靶子会让取消审计行的 channel 列写成 `headless_zcode`——那是假值）。
//! - API 为**逆向 bundle 所得（非公开文档）** ⇒ 版本门控覆盖 WB 升级漂移；本批的诚实形态是
//!   「端点不可用/协议不符即如实回执」，**绝不修改 WorkBuddy 安装本体**。
//!
//! # 测试纪律（宪法级）
//! 生产 HTTP 出口只有一处（[`production_http_seam`]，**`cfg(not(test))`**），测试构建里它
//! 根本不存在：端点用例的探针与 HTTP 缝都来自 [`test_hooks`]（默认：无心跳、无 WB 进程、
//! 任何 HTTP 请求一律**传输失败**）⇒ **单测零网络、零真实 `~/.workbuddy` 写入**（转写佐证
//! 走 tempdir 夹具）。wire 纯核全部经 [`MockHttp`] 注入缝驱动。
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::receipt::{Receipt, Stage};
// `ReceiptStatus` 只在测试面直取（生产路径只经 `Receipt::ok/failed/queued` 构造）
#[cfg(test)]
use super::receipt::ReceiptStatus;
use super::runner::GlobalSem;
use crate::inject::normalize;
use crate::monitor::workbuddy_parser::HeartbeatSnapshot;

// ============================================================
// HTTP 注入缝（**本任务内定义**；非外部依赖）
// ============================================================

/// ACP 建连端点（免鉴权）
pub const CONNECT_PATH: &str = "/api/v1/acp/connect";
/// ACP JSON-RPC 端点（逐请求一次 POST；响应/通知经 SSE 承载）
pub const ACP_PATH: &str = "/api/v1/acp";
/// `Accept` 头**逐字**（探测定案：缺 → `-32000 Not Acceptable: Client must accept both…`）
pub const ACCEPT_BOTH: &str = "application/json, text/event-stream";
/// ACP 协议版本（initialize 协商值；Mac 实测 1）
pub const ACP_PROTOCOL_VERSION: u32 = 1;
/// ACP 客户端身份（initialize 的 clientInfo.name；版本取本 crate 版本）
pub const CLIENT_NAME: &str = "mam-remote";

/// 流读取策略（**纯描述**：seam 只按帧边界与时钟执行，不解释 ACP 语义）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPolicy {
    /// 绝对上限（毫秒；生产 = H4 watchdog）
    pub max_ms: u64,
    /// `data` 帧静默上限（毫秒；**注释帧 `: heartbeat` 不算数据帧**——探测定案里
    /// 「已结束会话」的流正是「200 + 心跳但回合不推进」）
    pub idle_ms: u64,
    /// 整条流里**从未**出现该子串时，静默达 [`Self::idle_ms`] 即提前停（置
    /// [`StreamStop::Idle`]）；已见该子串则静默只受 [`Self::max_ms`] 约束
    pub idle_unless_seen: Option<&'static str>,
    /// 见到该 JSON-RPC `id` 的响应帧即停（响应即终：initialize/load/new/prompt 同理）
    pub stop_on_id: Option<u64>,
}

/// 流停止原因（**诚实标注**：谁掐断了流）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamStop {
    /// 自然结束或见到响应帧
    #[default]
    Complete,
    /// 静默死线（未见 `idle_unless_seen`）——探测定案的「已结束会话」形态
    Idle,
    /// 绝对上限（watchdog）
    Watchdog,
}

/// HTTP 请求（缝的输入；**纯描述**，不含任何 reqwest 类型——测试零依赖构造）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReq {
    pub method: &'static str,
    pub url: String,
    /// 头（名按 HTTP 语义**大小写不敏感**比较；值逐字）
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// 传输层超时（毫秒）
    pub timeout_ms: u64,
    pub stream: StreamPolicy,
}

impl HttpReq {
    /// 普通 JSON 请求（无流）
    pub fn json_post(url: String, headers: Vec<(String, String)>, body: String) -> Self {
        Self {
            method: "POST",
            url,
            headers,
            body: Some(body),
            timeout_ms: SHORT_TIMEOUT_MS,
            stream: StreamPolicy::whole(SHORT_TIMEOUT_MS),
        }
    }

    /// 指纹探测用 GET（短超时：本机回环、候选端口可能根本不是 HTTP）
    pub fn probe_get(url: String) -> Self {
        Self {
            method: "GET",
            url,
            headers: Vec::new(),
            body: None,
            timeout_ms: FINGERPRINT_TIMEOUT_MS,
            stream: StreamPolicy::whole(FINGERPRINT_TIMEOUT_MS),
        }
    }
}

impl StreamPolicy {
    /// 不读流（按普通请求读全 body）
    pub const fn whole(max_ms: u64) -> Self {
        Self {
            max_ms,
            idle_ms: 0,
            idle_unless_seen: None,
            stop_on_id: None,
        }
    }
}

/// 短请求传输超时（connect / 指纹探测之外的控制面请求）
pub const SHORT_TIMEOUT_MS: u64 = 20_000;
/// 指纹探测超时（候选端口可能是任意本地服务——必须有界；连接被拒即刻返回，只有
/// 「收 TCP 不答 HTTP」的服务才吃满这个超时）
pub const FINGERPRINT_TIMEOUT_MS: u64 = 1_000;
/// 端口候选上限（WB 是 Electron，监听端口不止一个；双确认前逐个试，**有界**）。
/// 最坏代价 = 上限 ×（`/` + `/health`）× 探测超时 ≈ 16s（只有候选端口全部「收 TCP
/// 不答 HTTP」时），典型（WB 未监听任何端口）= 0 次请求；**探测预算用尽即如实拒绝**
/// （宁可慢一点回绝，也不盲扫、不冒充）
pub const MAX_PORT_CANDIDATES: usize = 8;
/// 已结束会话判定死线（探测定案 + plan Step 1 原例逐字：60s 无 `agentPhase` 事件）
pub const PHASE_DEADLINE_MS: u64 = 60_000;
/// 短请求（initialize/load/new）的静默死线：响应帧即时到达，静默即异常
pub const CONTROL_IDLE_MS: u64 = 5_000;

/// HTTP 响应（缝的输出）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResp {
    /// 0 = 传输层失败（[`Self::error`] 必有值）
    pub status: u16,
    pub body: String,
    pub stop: StreamStop,
    pub error: Option<String>,
}

impl HttpResp {
    pub fn json(body: &str) -> Self {
        Self {
            status: 200,
            body: body.to_string(),
            stop: StreamStop::Complete,
            error: None,
        }
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.to_string(),
            stop: StreamStop::Complete,
            error: None,
        }
    }

    /// SSE 正常收流（含响应帧）
    pub fn sse(body: &str) -> Self {
        Self::json(body)
    }

    /// SSE 被静默死线掐断（探测定案：已结束会话的「200 + 心跳但回合不推进」）
    pub fn sse_stalled(body: &str) -> Self {
        Self {
            status: 200,
            body: body.to_string(),
            stop: StreamStop::Idle,
            error: None,
        }
    }

    /// SSE 被 watchdog 上限掐断
    pub fn sse_watchdog(body: &str) -> Self {
        Self {
            status: 200,
            body: body.to_string(),
            stop: StreamStop::Watchdog,
            error: None,
        }
    }

    pub fn transport_error(msg: &str) -> Self {
        Self {
            status: 0,
            body: String::new(),
            stop: StreamStop::Complete,
            error: Some(msg.to_string()),
        }
    }

    pub fn is_transport_error(&self) -> bool {
        self.error.is_some()
    }
}

/// HTTP 执行缝（**唯一 IO 出口**）；`Box<dyn Fn(HttpReq) -> HttpResp>` 的 Arc 封装
pub type HttpSeam = Arc<dyn Fn(HttpReq) -> HttpResp + Send + Sync>;

// ============================================================
// SSE / JSON-RPC 归一（纯核）
// ============================================================

/// 流证据（SSE → 归一产物；**未知帧只计数不解释**，与 Task 6 前缀跳过同纪律）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamEvidence {
    /// `agentPhase` 事件序列（首个 = [`Self::first_phase`]）
    pub phases: Vec<String>,
    /// 回合终点（`stopReason`；响应帧给出）
    pub stop_reason: Option<String>,
    /// 末条 assistant 文本（`session_update` 的 `agent_message_chunk`）
    pub assistant: Option<String>,
    /// 权限档（`config_option_update` 的 mode `currentValue`；**只读展示面未接线**）
    pub permission_mode: Option<String>,
    /// JSON-RPC 错误（`error.message`）
    pub rpc_error: Option<String>,
    /// `session_update: session_end` 在场（load 回放诊断用）
    pub session_end: bool,
    /// **含已知键**的帧数（被吸收解释的帧）
    pub frames: usize,
    /// JSON 但无任何已知键的帧数（**只计数不解释**——与 Task 6 前缀跳过同纪律）
    pub unknown_frames: usize,
    /// 注释帧数（`:ok` / `: heartbeat`——**不算推进证据**）
    pub comments: usize,
    /// 既非注释也非 JSON 的行数
    pub noise_lines: usize,
}

impl StreamEvidence {
    /// 首个 `agentPhase`（无 = 空串；plan Step 1 原例的断言面）
    pub fn first_phase(&self) -> String {
        self.phases.first().cloned().unwrap_or_default()
    }

    /// 回合是否推进过（见到 `agentPhase`）
    pub fn advanced(&self) -> bool {
        !self.phases.is_empty()
    }
}

/// SSE 体 → JSON 帧表（+ 注释/噪音分账）。
///
/// 帧边界按 SSE 规范：`data:` 行累积（多行以 `\n` 连接）、空行派发；`:` 起 = 注释帧
/// （`:ok` / `: heartbeat`——探测定案里「已结束会话」的流正是这些）；`event:` / `id:` /
/// `retry:` 行是元数据。**兜底**：既非注释也非 `data:` 的行若能解析为 JSON 也当帧收
/// （部分实现直接吐 JSON 行）；否则记噪音行（不猜）。
fn parse_frames(body: &str) -> (Vec<serde_json::Value>, usize, usize) {
    let mut frames: Vec<serde_json::Value> = Vec::new();
    let mut comments = 0usize;
    let mut noise = 0usize;
    let mut data = String::new();
    let flush = |data: &mut String, frames: &mut Vec<serde_json::Value>, noise: &mut usize| {
        let payload = data.trim();
        if !payload.is_empty() {
            match serde_json::from_str::<serde_json::Value>(payload) {
                Ok(v) => frames.push(v),
                Err(_) => *noise += 1,
            }
        }
        data.clear();
    };
    for raw in body.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            flush(&mut data, &mut frames, &mut noise);
            continue;
        }
        if line.starts_with(':') {
            comments += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
            continue;
        }
        if line.starts_with("event:") || line.starts_with("id:") || line.starts_with("retry:") {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => frames.push(v),
            Err(_) => noise += 1,
        }
    }
    flush(&mut data, &mut frames, &mut noise);
    (frames, comments, noise)
}

/// 递归取首个同名字符串值（**只认已知键**；找不到 = None，不猜）
fn deep_str(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(s) = map.get(key).and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
            map.values().find_map(|x| deep_str(x, key))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|x| deep_str(x, key)),
        _ => None,
    }
}

/// 吸收一帧（返回是否含**已知键**——未知帧只计数不解释）
fn absorb(v: &serde_json::Value, ev: &mut StreamEvidence) -> bool {
    let mut known = false;
    if let Some(msg) = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        ev.rpc_error = Some(msg.to_string());
        known = true;
    }
    if let Some(phase) = deep_str(v, "agentPhase") {
        ev.phases.push(phase);
        known = true;
    }
    if let Some(kind) = deep_str(v, "sessionUpdate") {
        if kind == "session_end" {
            ev.session_end = true;
            known = true;
        }
        if kind == "agent_message_chunk" {
            if let Some(text) = deep_str(v, "text") {
                ev.assistant = Some(text);
                known = true;
            }
        }
    }
    if let Some(stop) = deep_str(v, "stopReason") {
        ev.stop_reason = Some(stop);
        known = true;
    }
    if deep_str(v, "category").as_deref() == Some("mode") {
        if let Some(cur) = deep_str(v, "currentValue") {
            ev.permission_mode = Some(cur);
            known = true;
        }
    }
    known
}

/// SSE 体 → 流证据（`data:` 逐行累积 + 裸 JSON 行兜底；注释/未知帧/噪音分账）
pub fn read_stream(body: &str) -> StreamEvidence {
    let (frames, comments, noise) = parse_frames(body);
    let mut ev = StreamEvidence {
        comments,
        noise_lines: noise,
        ..Default::default()
    };
    for f in &frames {
        if absorb(f, &mut ev) {
            ev.frames += 1;
        } else {
            ev.unknown_frames += 1;
        }
    }
    ev
}

// ============================================================
// 端点发现（心跳 → 端口指纹 → 如实不可用）
// ============================================================

/// 端点来源（回执/审计的可诊断面）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointSource {
    /// 心跳 `endpoint` 字段（macOS/旧版；每会话异端口）
    Heartbeat { pid: u32 },
    /// 端口指纹双确认（Windows 启用远程控制后）
    PortFingerprint { pid: u32, port: u16 },
}

/// 端点发现结论
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointDiscovery {
    Found {
        url: String,
        source: EndpointSource,
    },
    /// **如实不可用**（不盲扫）：原因里必须带出**启用条件**
    Unavailable {
        reason: String,
    },
}

impl EndpointDiscovery {
    pub fn url(&self) -> Option<&str> {
        match self {
            EndpointDiscovery::Found { url, .. } => Some(url),
            EndpointDiscovery::Unavailable { .. } => None,
        }
    }
}

/// 「端点在不在」的探测依赖（**全注入**：心跳读 / WB 宿主进程表 / 监听端口表）
pub struct EndpointProbe {
    /// 平台（`std::env::consts::OS` 原值；端口指纹路只在 Windows 走——探测定案）
    pub os: &'static str,
    /// 心跳快照（生产 = 读 `~/.workbuddy/sessions/<pid>.json`；测试 = 脚本桩）
    pub heartbeat: Box<dyn Fn(u32) -> Option<HeartbeatSnapshot> + Send + Sync>,
    /// WB **宿主**进程表（生产 = sysinfo 按 exe 名；测试 = 脚本桩）
    pub wb_pids: Box<dyn Fn() -> Vec<u32> + Send + Sync>,
    /// pid 的监听端口表（生产 = `Get-NetTCPConnection -State Listen -OwningProcess`）
    pub listening_ports: Box<dyn Fn(u32) -> Vec<u16> + Send + Sync>,
}

/// 端点不可用的**统一文案**（plan Step 3 逐字：启用条件 + 风险 16 跟进项）。
///
/// **阶段词选择（义务 4，诚实裁决）**：文案里的 `channel_unavailable` 是**原因码**，
/// **不新增 `Stage` 变体**——端点未启用时回合**未起跑、零字节投递**，正是既有
/// [`Stage::Refused`]（投递前拒绝）的定义；`ChannelError` 的语义是「通道跑过了但没拿到
/// 有效回执」，用在这里会让移动端分诊说错话（把「没发出去」显示成「通道异常」）。
/// 故回执 = `failed(refused)` + 本文案（含原因码），跨语言夹具
/// `tests/fixtures/headless_stages.json` **无须改动**（无新阶段）。
pub const ENDPOINT_UNAVAILABLE_REASON: &str =
    "WorkBuddy 远程控制端点未启用（channel_unavailable）——请在 WorkBuddy 设置中开启远程控制（风险 16 跟进项）";
/// `/` 标题指纹（探测定案：CodeBuddy Remote Control）
pub const TITLE_FINGERPRINT: &str = "CodeBuddy Remote Control";

/// 回环端点校验（**安全铁律**：用户正文只许 POST 到本机回环——心跳文件是私有格式，
/// 非回环形态一律丢弃，不猜、不外发）
pub fn loopback_url(raw: &str) -> Option<String> {
    let s = raw.trim();
    let rest = s.strip_prefix("http://")?;
    // 去路径/查询/片段，只留 authority
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None; // 拒绝 userinfo（`http://evil@127.0.0.1` 一类混淆）
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    let ok = matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    );
    ok.then(|| s.trim_end_matches('/').to_string())
}

/// `/health` 双确认之一：体须为 `{"status":"UP"}`（严格 JSON 解析——不猜）
pub fn health_ok(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()
        .and_then(|v| {
            v.get("status")
                .and_then(|s| s.as_str())
                .map(|s| s.eq_ignore_ascii_case("up"))
        })
        .unwrap_or(false)
}

/// 端口指纹**双确认**（标题指纹 + `/health` UP——单侧命中一律不认，防把别的本地服务
/// 当 WorkBuddy 远程控制端点）
pub fn fingerprint_hit(root_body: &str, health_body: &str) -> bool {
    root_body
        .to_lowercase()
        .contains(&TITLE_FINGERPRINT.to_lowercase())
        && health_ok(health_body)
}

/// `Get-NetTCPConnection | Select -ExpandProperty LocalPort` 输出 → 端口表（纯核；
/// 表头/噪声/越界值一律跳过，不猜）
pub fn parse_ports(text: &str) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Ok(port) = t.parse::<u16>() {
            if port > 0 && !out.contains(&port) {
                out.push(port);
            }
        }
    }
    out
}

/// WB 宿主进程判定（**只服务端口发现**：要 APP 的 pid 去问它监听了什么；会话发现
/// **禁用进程名匹配**——见 `monitor::workbuddy_parser` 模块文档）
pub fn is_wb_host_exe(exe: Option<&Path>) -> bool {
    exe.and_then(|e| e.file_name())
        .map(|n| n.to_string_lossy().to_lowercase().contains("workbuddy"))
        .unwrap_or(false)
}

/// 端口指纹双确认（GET `/` + GET `/health`，**都走注入缝**）
fn fingerprint_port(http: &HttpSeam, root: &str) -> Option<String> {
    let root_resp = http(HttpReq::probe_get(format!("{root}/")));
    if root_resp.error.is_some() {
        return None; // 连接失败/超时：本候选不认（继续下一个）
    }
    let health = http(HttpReq::probe_get(format!("{root}/health")));
    if health.error.is_some() {
        return None;
    }
    fingerprint_hit(&root_resp.body, &health.body).then_some(root.to_string())
}

/// 端点发现内核（**心跳优先 → Windows 端口指纹 → 如实不可用**）。
///
/// 心跳路要求四件事同时成立（Mac/旧版形态）：心跳**可用**（严格 UUID + 非 prewarm +
/// 新鲜 < 90s + 文件名/内容 pid 一致）、**会话号匹配**（每会话异端口）、`endpoint` 在场、
/// 且为**回环**形态。Windows 5.7.3（无交互心跳）→ 端口指纹路：WB 宿主 pid 过滤 →
/// 逐候选端口 `/` 标题指纹 + `/health` **双确认**（候选有界）。
/// 两路都落空 ⇒ 如实 [`EndpointDiscovery::Unavailable`]，**不盲扫**。
pub fn discover_endpoint(
    probe: &EndpointProbe,
    http: &HttpSeam,
    session_id: &str,
    pid: u32,
) -> EndpointDiscovery {
    // ① 心跳路
    let mut detail = if pid == 0 {
        "会话无宿主进程（pid=0，db 源哨兵卡）且无心跳".to_string()
    } else {
        match (probe.heartbeat)(pid) {
            Some(hb) if hb.usable && hb.session_id == session_id => {
                match hb.endpoint.as_deref().and_then(loopback_url) {
                    Some(url) => {
                        return EndpointDiscovery::Found {
                            url,
                            source: EndpointSource::Heartbeat { pid },
                        }
                    }
                    None => {
                        "心跳在场但 endpoint 缺失或非回环（已丢弃——绝不把正文发出本机）".to_string()
                    }
                }
            }
            Some(hb) if hb.usable => {
                "在场心跳属于别的会话（ACP 端点按会话隔离，不借他会话端点）".to_string()
            }
            Some(_) => "该会话心跳不可用（已过期/pid 不符/prewarm）——陈旧心跳的 endpoint 一律不用"
                .to_string(),
            None => "该会话无心跳文件".to_string(),
        }
    };
    // ② 端口指纹路（**只在 Windows**：探测定案的平台差异——macOS 走心跳即可）
    if probe.os == "windows" {
        let pids = (probe.wb_pids)();
        let mut tried = 0usize;
        'outer: for wb_pid in &pids {
            for port in (probe.listening_ports)(*wb_pid) {
                if tried >= MAX_PORT_CANDIDATES {
                    break 'outer;
                }
                tried += 1;
                let root = format!("http://127.0.0.1:{port}");
                if let Some(url) = fingerprint_port(http, &root) {
                    return EndpointDiscovery::Found {
                        url,
                        source: EndpointSource::PortFingerprint { pid: *wb_pid, port },
                    };
                }
            }
        }
        let host = if pids.is_empty() {
            "WB 宿主进程不在场"
        } else {
            "WB 宿主在场"
        };
        detail = format!("{detail}；端口指纹路：{host}，候选 {tried} 个端口双确认 0 命中");
    }
    EndpointDiscovery::Unavailable {
        reason: format!("{ENDPOINT_UNAVAILABLE_REASON}；本次探测：{detail}"),
    }
}

// ============================================================
// ACP 客户端（wire 纯核；**测试唯一入口 = MockHttp**）
// ============================================================

/// 连接态（connect 产物）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpSession {
    pub connection_id: String,
    pub token: String,
}

/// 能力协商结论（initialize 产物）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub protocol_version: Option<u32>,
    /// `agentCapabilities.loadSession`（false ⇒ 活跃会话不可 load，如实拒绝）
    pub load_session: bool,
}

/// `session/load` 回放证据（**诊断用**，不是投递前置）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadOutcome {
    /// 回放里末回合的 `stopReason`（非 None ⇒ 该会话上一次回合已结束）
    pub last_stop_reason: Option<String>,
    /// 权限档（只读；切换留三期 F3.1）
    pub permission_mode: Option<String>,
    /// 回放被静默死线掐断（历史很长时可能；不阻塞 prompt）
    pub truncated: bool,
}

/// 目标语义二分（**协议核**）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSel<'a> {
    /// 活跃会话：`session/load` + `session/prompt`
    Active { session_id: &'a str, cwd: &'a str },
    /// 新建：`session/new` + `session/prompt`（**本批无生产调用者**——见模块文档；
    /// 端点的 WB「新建会话」入口不在本批范围）
    New { cwd: &'a str },
}

/// 目标定位产物
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// 实际投递的 ACP 会话号（`New` 时 = `session/new` 新建的 id）
    pub session_id: String,
    /// 是否新建
    pub created: bool,
    /// `session/load` 回放证据（`New` 时 None）
    pub load: Option<LoadOutcome>,
}

/// prompt 流判定产物
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptOutcome {
    pub session_id: String,
    /// 首个 `agentPhase`（无 = 空串）
    pub first_phase: String,
    pub phases: Vec<String>,
    pub stop_reason: Option<String>,
    pub last_assistant: Option<String>,
    pub permission_mode: Option<String>,
    /// 流停止原因（诚实标注谁掐断的）
    pub stop: StreamStop,
    pub evidence: StreamEvidence,
}

impl PromptOutcome {
    pub fn advanced(&self) -> bool {
        !self.phases.is_empty()
    }
}

/// ACP 客户端（**唯一 IO 出口 = 注入缝**；生产缝见 [`production_http_seam`]）
pub struct AcpClient {
    base: String,
    http: HttpSeam,
    next_id: std::sync::atomic::AtomicU64,
    watchdog_ms: u64,
}

/// ACP `POST /api/v1/acp` 的三件套头（**逐字**；头名按 HTTP 语义大小写不敏感）
fn acp_headers(sess: &AcpSession) -> Vec<(String, String)> {
    vec![
        ("accept".to_string(), ACCEPT_BOTH.to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("acp-connection-id".to_string(), sess.connection_id.clone()),
        ("acp-session-token".to_string(), sess.token.clone()),
    ]
}

/// RPC 响应读取结论：`Ok(Some(v))` = 该 id 的结果；`Ok(None)` = **无响应帧**（流已尽力，
/// 判定交给调用方）；`Err(_)` = 服务端显式错误（错误帧 / `error` 对象 / 非 2xx）
fn rpc_result(id: u64, resp: &HttpResp) -> Result<Option<serde_json::Value>, String> {
    if let Some(e) = &resp.error {
        return Err(e.clone());
    }
    // 显式错误优先（服务端明确拒绝；含 SSE 帧里的 `error.message`）
    let (frames, _, _) = parse_frames(&resp.body);
    for frame in &frames {
        for candidate in [Some(frame), frame.get("error")].into_iter().flatten() {
            if let Some(msg) = candidate
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
            {
                return Err(msg.to_string());
            }
        }
    }
    if !(200..300).contains(&resp.status) && resp.status != 0 {
        let brief = normalize::summarize(&resp.body, normalize::AUDIT_SUMMARY_CHARS);
        return Err(format!("HTTP {}：{brief}", resp.status));
    }
    if let Some(hit) = frames
        .iter()
        .find(|f| f.get("id").and_then(|x| x.as_u64()) == Some(id))
    {
        if let Some(result) = hit.get("result") {
            return Ok(Some(result.clone()));
        }
    }
    Ok(None)
}

impl AcpClient {
    /// `base` = 端点根（如 `http://127.0.0.1:63928`）；`http` = 注入缝
    /// （测试传 [`MockHttp`]，生产传 [`production_http_seam`]）
    pub fn new<H: Into<HttpSeam>>(base: &str, http: H) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: http.into(),
            next_id: std::sync::atomic::AtomicU64::new(1),
            watchdog_ms: super::DEFAULT_TIMEOUT_MS,
        }
    }

    /// 回合 watchdog（决定流读取绝对上限；生产 = H4 设置）
    pub fn watchdog_ms(mut self, ms: u64) -> Self {
        self.watchdog_ms = super::clamp_timeout_ms(ms);
        self
    }

    fn next_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// 逐请求一次 `POST /api/v1/acp`（三头 + JSON-RPC 体；流策略由调用方给）
    fn acp_post(
        &self,
        sess: &AcpSession,
        id: u64,
        method: &str,
        params: serde_json::Value,
        policy: StreamPolicy,
    ) -> Result<HttpResp, String> {
        let body =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                .to_string();
        let resp = (self.http)(HttpReq {
            method: "POST",
            url: format!("{}{}", self.base, ACP_PATH),
            headers: acp_headers(sess),
            body: Some(body),
            timeout_ms: policy.max_ms.max(SHORT_TIMEOUT_MS),
            stream: policy,
        });
        if let Some(e) = &resp.error {
            return Err(format!("{method} 请求未送达：{e}"));
        }
        Ok(resp)
    }

    /// `POST /api/v1/acp/connect`（**免鉴权**）→ 连接态
    pub fn handshake(&self) -> Result<AcpSession, String> {
        let resp = (self.http)(HttpReq {
            method: "POST",
            url: format!("{}{}", self.base, CONNECT_PATH),
            headers: Vec::new(),
            body: None,
            timeout_ms: SHORT_TIMEOUT_MS,
            stream: StreamPolicy::whole(SHORT_TIMEOUT_MS),
        });
        if let Some(e) = &resp.error {
            return Err(format!("建连请求未送达：{e}"));
        }
        if !(200..300).contains(&resp.status) {
            return Err(format!(
                "建连 HTTP {}：{}",
                resp.status,
                normalize::summarize(&resp.body, normalize::AUDIT_SUMMARY_CHARS)
            ));
        }
        let v: serde_json::Value = serde_json::from_str(resp.body.trim()).map_err(|e| {
            format!(
                "建连响应不是 JSON：{e}；体={}",
                normalize::summarize(&resp.body, normalize::AUDIT_SUMMARY_CHARS)
            )
        })?;
        let connection_id = v
            .get("connectionId")
            .and_then(|x| x.as_str())
            .ok_or_else(|| {
                "建连响应缺 connectionId（API 为逆向 bundle 所得，WorkBuddy 升级可能漂移）"
                    .to_string()
            })?;
        let token = v
            .get("sessionToken")
            .and_then(|x| x.as_str())
            .ok_or_else(|| {
                "建连响应缺 sessionToken（同上：逆向 API 的版本漂移风险）".to_string()
            })?;
        Ok(AcpSession {
            connection_id: connection_id.to_string(),
            token: token.to_string(),
        })
    }

    /// `initialize`（能力协商）
    pub fn initialize(&self, sess: &AcpSession) -> Result<Capabilities, String> {
        let id = self.next_id();
        let params = serde_json::json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
            "clientCapabilities": {},
        });
        let policy = StreamPolicy {
            max_ms: self.watchdog_ms,
            idle_ms: CONTROL_IDLE_MS,
            idle_unless_seen: None,
            stop_on_id: Some(id),
        };
        let resp = self.acp_post(sess, id, "initialize", params, policy)?;
        let result = rpc_result(id, &resp)
            .map_err(|e| format!("initialize 被服务端拒绝：{e}"))?
            .ok_or_else(|| {
                "initialize 未拿到响应帧（探测定案：缺 Accept 头即 -32000）".to_string()
            })?;
        Ok(Capabilities {
            protocol_version: result
                .get("protocolVersion")
                .and_then(|x| x.as_u64())
                .map(|v| v as u32),
            // 字段缺席 = 不支持（ACP 语义：loadSession 是可选能力）——保守判，不乐观默认
            load_session: result
                .get("agentCapabilities")
                .and_then(|a| a.get("loadSession"))
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
        })
    }

    /// 目标定位（initialize + 二分：`session/load` 或 `session/new`）
    pub fn open_target(&self, sess: &AcpSession, sel: &SessionSel<'_>) -> Result<Target, String> {
        let caps = self.initialize(sess)?;
        match sel {
            SessionSel::Active { session_id, cwd } => {
                if !caps.load_session {
                    return Err(
                        "服务端 agentCapabilities.loadSession=false：活跃会话不可 session/load\
                         （API 逆向所得，WorkBuddy 升级可能漂移）"
                            .to_string(),
                    );
                }
                let id = self.next_id();
                let params = serde_json::json!({
                    "sessionId": session_id,
                    "cwd": cwd,
                    "mcpServers": [],
                });
                let policy = StreamPolicy {
                    max_ms: self.watchdog_ms,
                    idle_ms: CONTROL_IDLE_MS,
                    idle_unless_seen: None,
                    stop_on_id: Some(id),
                };
                let resp = self.acp_post(sess, id, "session/load", params, policy)?;
                let result =
                    rpc_result(id, &resp).map_err(|e| format!("session/load 被拒：{e}"))?;
                let ev = read_stream(&resp.body);
                // 无响应帧但**有回放证据**：属 best-effort（load 只作诊断，投递判定归 prompt）
                if result.is_none()
                    && !ev.session_end
                    && ev.permission_mode.is_none()
                    && ev.frames == 0
                {
                    return Err("session/load 未拿到响应帧也无回放证据".to_string());
                }
                Ok(Target {
                    session_id: (*session_id).to_string(),
                    created: false,
                    load: Some(LoadOutcome {
                        last_stop_reason: ev.stop_reason.clone(),
                        permission_mode: ev.permission_mode.clone(),
                        truncated: resp.stop != StreamStop::Complete,
                    }),
                })
            }
            SessionSel::New { cwd } => {
                let id = self.next_id();
                let params = serde_json::json!({ "cwd": cwd, "mcpServers": [] });
                let policy = StreamPolicy {
                    max_ms: self.watchdog_ms,
                    idle_ms: CONTROL_IDLE_MS,
                    idle_unless_seen: None,
                    stop_on_id: Some(id),
                };
                let resp = self.acp_post(sess, id, "session/new", params, policy)?;
                let result = rpc_result(id, &resp)
                    .map_err(|e| format!("session/new 被拒：{e}"))?
                    .ok_or_else(|| "session/new 未拿到响应帧".to_string())?;
                let new_sid = result
                    .get("sessionId")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "session/new 响应缺 sessionId".to_string())?;
                Ok(Target {
                    session_id: new_sid.to_string(),
                    created: true,
                    load: None,
                })
            }
        }
    }

    /// 在已定位目标上投递 prompt（编排用：先取 load 证据再发）。
    ///
    /// 流策略是**诚实性的关键**：静默死线 60s（[`PHASE_DEADLINE_MS`]）且**只在从未出现
    /// `agentPhase` 时生效**（已见 phase ⇒ 回合真的在跑，静默只受 watchdog 约束）；
    /// 见到与本次 id 对应的响应帧（回合终点 `stopReason`）即收流。
    pub fn prompt_on(
        &self,
        sess: &AcpSession,
        target: &Target,
        text: &str,
    ) -> Result<PromptOutcome, String> {
        let id = self.next_id();
        let params = serde_json::json!({
            "sessionId": target.session_id,
            "prompt": [{"type": "text", "text": text}],
        });
        let policy = StreamPolicy {
            max_ms: self.watchdog_ms,
            idle_ms: PHASE_DEADLINE_MS,
            idle_unless_seen: Some("agentPhase"),
            stop_on_id: Some(id),
        };
        let resp = self.acp_post(sess, id, "session/prompt", params, policy)?;
        rpc_result(id, &resp).map_err(|e| format!("session/prompt 被拒：{e}"))?;
        let ev = read_stream(&resp.body);
        Ok(PromptOutcome {
            session_id: target.session_id.clone(),
            first_phase: ev.first_phase(),
            phases: ev.phases.clone(),
            stop_reason: ev.stop_reason.clone(),
            last_assistant: ev.assistant.clone(),
            permission_mode: ev
                .permission_mode
                .clone()
                .or_else(|| target.load.as_ref().and_then(|l| l.permission_mode.clone())),
            stop: resp.stop,
            evidence: ev,
        })
    }

    /// 一次性便捷口（plan Step 1 原例形状）：initialize + 二分 + prompt
    pub fn prompt(
        &self,
        sess: &AcpSession,
        text: &str,
        sel: SessionSel<'_>,
    ) -> Result<PromptOutcome, String> {
        let target = self.open_target(sess, &sel)?;
        self.prompt_on(sess, &target, text)
    }
}

// ============================================================
// 判定与回执（纯核）
// ============================================================

/// prompt 流判定
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptVerdict {
    /// 回合推进并拿到终点（响应帧 `stopReason`）
    Completed { stop_reason: String },
    /// 推进了但没拿到终点（`stop` 说明被谁掐断）——**不冒充成功**
    Advanced { phase: String, stop: StreamStop },
    /// 未推进（无 `agentPhase` 且无终点）⇒ **已结束需复活**（探测定案的静默挂形态）
    RevivalNeeded { truncated: bool },
    /// 明确失败（RPC 错误 / 传输错误）
    Failed { detail: String },
}

/// 「需复活」回执文案骨架（plan Step 1 原例逐字意图：说清已结束 + 引导重开或新建）。
/// 「已结束」是**从静默推断**（见 [`judge_prompt`]）——文案以「需复活语义」表达，不声称
/// 服务端报告了会话终态。
pub const REVIVAL_REASON: &str = "会话已结束（end_turn），需复活语义——ACP `session/prompt` 对已结束会话只接收不执行（接口语义，非鉴权）：请在 WorkBuddy 应用内重开该会话后再发，或走新建会话";

/// 未拿到终点的如实文案（**不冒充成功**）
pub const UNCONFIRMED_REASON: &str =
    "ACP 回合已推进但未拿到终点回执（流结束/被掐断）——请在 WorkBuddy 应用内确认结果后再决定是否重发";

/// prompt 流判定（纯核）：
/// - 显式 RPC 错误 → [`PromptVerdict::Failed`]；
/// - 终点在场（`stopReason`）→ [`PromptVerdict::Completed`]；
/// - 有 `agentPhase` 无终点 → [`PromptVerdict::Advanced`]（**不冒充成功**：watchdog 掐断
///   记 timeout，流自己断了记 channel_error）；
/// - **既无 `agentPhase` 也无终点 → [`PromptVerdict::RevivalNeeded`]**（探测定案的
///   「已结束会话静默挂」形态；`truncated` = 是否由 60s 静默死线判出，供回执措辞）。
///
/// **这是「推断」而非「观测」（登记，Task 11 复审）**：ACP 不给「该会话已结束」的显式
/// 信号（load 回放的 `session_end` 判不出活跃/已结束——跑完过一回合的会话回放都有它），
/// 故「已结束」是**从静默推断**出来的：60s 内没有任何 `agentPhase`。措辞与回执据此如实
/// 表达（「需复活语义」而非「服务端报告会话已结束」）。
pub fn judge_prompt(out: &PromptOutcome) -> PromptVerdict {
    if let Some(detail) = &out.evidence.rpc_error {
        return PromptVerdict::Failed {
            detail: detail.clone(),
        };
    }
    if let Some(stop) = &out.stop_reason {
        return PromptVerdict::Completed {
            stop_reason: stop.clone(),
        };
    }
    if out.advanced() {
        return PromptVerdict::Advanced {
            phase: out.first_phase.clone(),
            stop: out.stop,
        };
    }
    PromptVerdict::RevivalNeeded {
        truncated: out.stop != StreamStop::Complete,
    }
}

/// 转写佐证（`projects/` 落盘侦测；**二次确认**，不作唯一证据）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Corroboration {
    /// 转写文件在场
    pub present: bool,
    /// mtime 相对回合前基线推进（主判据）
    pub advanced: bool,
    /// 尾部命中本次载荷标记（次级判据）
    pub needle_hit: bool,
    pub path: Option<PathBuf>,
}

impl Corroboration {
    /// 是否已确认（两判据任一）
    pub fn confirmed(&self) -> bool {
        self.advanced || self.needle_hit
    }
}

/// 载荷佐证标记（W4 尾签名优先——可读且稳定；无签名取正文前 16 字符）
pub fn corroboration_needle(payload: &str) -> String {
    match payload.rfind("[mobile ") {
        Some(idx) => payload[idx..].trim_end().to_string(),
        None => payload.trim().chars().take(16).collect(),
    }
}

fn mtime_ms_of(path: &Path) -> Option<u64> {
    path.metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// 转写 mtime（毫秒；文件不在/不可读 → None）。定位复用读侧
/// `workbuddy_parser::find_session_jsonl`（**不另写转写定位**）
pub fn transcript_mtime_ms(home: Option<&Path>, cwd: &str, session_id: &str) -> Option<u64> {
    let path = crate::monitor::workbuddy_parser::find_session_jsonl(home?, cwd, session_id)?;
    mtime_ms_of(&path)
}

/// 转写佐证（`projects/` 落盘侦测；**二次确认**，不作唯一证据——ACP 流的 `stopReason`
/// 才是回合终点的主证据，转写可能滞后落盘）。
/// 判据：mtime 相对回合前基线**推进**（主）∪ 尾部命中本次载荷标记（次，复用读侧
/// `monitor::jsonl` 尾部读取——**不另写转写解析**）。
pub fn corroborate(
    home: Option<&Path>,
    cwd: &str,
    session_id: &str,
    payload: &str,
    baseline_mtime_ms: Option<u64>,
) -> Corroboration {
    let Some(home) = home else {
        return Corroboration::default();
    };
    let Some(path) = crate::monitor::workbuddy_parser::find_session_jsonl(home, cwd, session_id)
    else {
        return Corroboration::default();
    };
    let now = mtime_ms_of(&path);
    let advanced = match (baseline_mtime_ms, now) {
        (Some(base), Some(now)) => now > base,
        _ => false,
    };
    let needle = corroboration_needle(payload);
    let needle_hit = !needle.is_empty()
        && crate::monitor::jsonl::read_recent_lines(&path, 200)
            .iter()
            .any(|l| l.contains(&needle));
    Corroboration {
        present: true,
        advanced,
        needle_hit,
        path: Some(path),
    }
}

/// 回执归一（判定 + 转写佐证 → H6 回执）。**诚实铁律**：
/// - 只有拿到回合终点（`stopReason`）才可能 `ok`——且转写佐证未命中时在 `reason` 里如实标注；
/// - 未推进 → `failed(channel_error)` + [`REVIVAL_REASON`]（引导重开或新建）；
/// - 推进但无终点 → watchdog 掐断记 `timeout`，流自己断了记 `channel_error`（都带未确认文案）。
pub fn receipt_for(
    sid: &str,
    verdict: &PromptVerdict,
    out: &PromptOutcome,
    load: Option<&LoadOutcome>,
    corr: &Corroboration,
    duration_ms: u64,
) -> Receipt {
    let used = if out.session_id.is_empty() {
        sid
    } else {
        out.session_id.as_str()
    };
    match verdict {
        PromptVerdict::Completed { stop_reason } => {
            let mut r = Receipt::ok(used, duration_ms);
            r.last_assistant = out
                .last_assistant
                .as_deref()
                .map(|t| normalize::summarize(t, super::receipt::LAST_ASSISTANT_CHARS));
            if !corr.confirmed() {
                r.reason = Some(format!(
                    "ACP 回合已结束（stopReason={stop_reason}），但 projects/ 转写佐证未命中\
                     （转写落盘可能滞后）——请在 WorkBuddy 应用内确认结果"
                ));
            }
            r
        }
        PromptVerdict::Advanced { phase, stop } => {
            let (stage, how) = match stop {
                StreamStop::Watchdog => (Stage::Timeout, "watchdog 到点掐断"),
                _ => (Stage::ChannelError, "流已结束"),
            };
            Receipt::failed(
                stage,
                &format!("{UNCONFIRMED_REASON}（{how}；已见 agentPhase={phase}）"),
            )
            .with_session(used)
            .with_duration_ms(duration_ms)
        }
        PromptVerdict::RevivalNeeded { truncated } => {
            let obs = if *truncated {
                "prompt 后 60s 内无 agentPhase 事件（探测定案的静默挂形态）"
            } else {
                "prompt 流内始终没有 agentPhase 事件"
            };
            let prev = load
                .and_then(|l| l.last_stop_reason.as_deref())
                .map(|s| format!("；load 回放显示末回合 stopReason={s}"))
                .unwrap_or_default();
            Receipt::failed(
                Stage::ChannelError,
                &format!("{obs}{prev}；{REVIVAL_REASON}"),
            )
            .with_session(used)
            .with_duration_ms(duration_ms)
        }
        PromptVerdict::Failed { detail } => Receipt::failed(
            Stage::ChannelError,
            &format!("WorkBuddy ACP 投递失败：{detail}"),
        )
        .with_session(used)
        .with_duration_ms(duration_ms),
    }
}

// ============================================================
// 单回合编排（Task 6/9 底座的唯一消费入口）
// ============================================================

/// 回合参数（自有数据——`spawn_blocking` 闭包要求 `'static`）
#[derive(Debug, Clone)]
pub struct WbTurnArgs {
    /// MAM 会话号
    pub sid: String,
    /// 会话项目目录（ACP `session/load`/`new` 的 cwd）
    pub project: String,
    /// W4 组装后的最终载荷（审计 content 与 ACP prompt 同源）
    pub payload: String,
    /// 会话宿主 pid（0 = 无宿主：db 源哨兵卡）
    pub pid: u32,
}

/// 编排依赖（**全注入**：探针 / HTTP 缝 / 转写 home / 名额 / watchdog）
pub struct WbDeps {
    /// 全局并发名额（**生产 = [`super::runner::global_sem`]**——不自建 `GlobalSem`）
    pub sem: GlobalSem,
    /// 回合 watchdog（生产 = H4 设置经 `headless::runner_from_conn` 读取）
    pub watchdog_ms: u64,
    pub probe: EndpointProbe,
    /// **唯一 IO 出口**
    pub http: HttpSeam,
    /// 转写 home（测试注入 tempdir/None，**绝不触真实 `~/.workbuddy`**）
    pub home: Option<PathBuf>,
}

/// 回合结局
#[derive(Debug, Clone, PartialEq)]
pub struct WbTurnOutcome {
    pub receipt: Receipt,
    /// 端点发现结论（未起跑 = None）
    pub endpoint: Option<EndpointDiscovery>,
    /// prompt 流判定（未起跑 = None）
    pub verdict: Option<PromptVerdict>,
    pub corroboration: Corroboration,
}

impl WbTurnOutcome {
    /// 未起跑（排队/拒绝）的结局
    fn not_started(receipt: Receipt) -> Self {
        Self {
            receipt,
            endpoint: None,
            verdict: None,
            corroboration: Corroboration::default(),
        }
    }
}

/// 未起跑/失败的结局（端点结论照带——回执可诊断）
fn failed_outcome(
    sid: &str,
    discovery: &EndpointDiscovery,
    detail: String,
    duration_ms: u64,
) -> WbTurnOutcome {
    WbTurnOutcome {
        receipt: Receipt::failed(Stage::ChannelError, &detail)
            .with_session(sid)
            .with_duration_ms(duration_ms),
        endpoint: Some(discovery.clone()),
        verdict: None,
        corroboration: Corroboration::default(),
    }
}

/// 单回合编排（**同步**：HTTP 缝是阻塞的，调用方必须放 `spawn_blocking`——绝不在
/// async 运行时线程上阻塞）。顺序：并发名额 → 端点发现 → connect → initialize →
/// 二分（load/new）→ prompt → 判定 → 转写佐证 → 回执。
///
/// **取消**：本通道不武装取消靶子（见模块文档）——槽位由[`super::turn::TurnRegistry`]
/// 持有作会话串行锁，取消端点对 WB 回合如实报「未送达」。
pub fn run_turn(args: &WbTurnArgs, deps: &WbDeps) -> WbTurnOutcome {
    let started = Instant::now();
    let elapsed = |t: &Instant| t.elapsed().as_millis() as u64;
    // ① 全局并发名额（H4：**不自建 `GlobalSem`**；超额即时排队回执，不阻塞、不静默丢）
    let Some(_permit) = deps.sem.try_acquire() else {
        return WbTurnOutcome::not_started(
            Receipt::queued(deps.sem.queue_position()).with_session(&args.sid),
        );
    };
    // ② 端点发现（心跳 → 端口指纹 → **如实不可用**；不可用 = 投递前拒绝、零 HTTP）
    let discovery = discover_endpoint(&deps.probe, &deps.http, &args.sid, args.pid);
    let Some(url) = discovery.url().map(str::to_string) else {
        let reason = match &discovery {
            EndpointDiscovery::Unavailable { reason } => reason.clone(),
            EndpointDiscovery::Found { .. } => String::new(),
        };
        return WbTurnOutcome::not_started(
            Receipt::failed(Stage::Refused, &reason).with_session(&args.sid),
        );
    };
    // ③ ACP 全链（connect → initialize → 二分 → prompt）
    let client = AcpClient::new(&url, deps.http.clone()).watchdog_ms(deps.watchdog_ms);
    let sess = match client.handshake() {
        Ok(s) => s,
        Err(e) => {
            return failed_outcome(
                &args.sid,
                &discovery,
                format!("WorkBuddy ACP 建连失败（{url}）：{e}"),
                elapsed(&started),
            )
        }
    };
    let sel = SessionSel::Active {
        session_id: &args.sid,
        cwd: &args.project,
    };
    let target = match client.open_target(&sess, &sel) {
        Ok(t) => t,
        Err(e) => {
            return failed_outcome(
                &args.sid,
                &discovery,
                format!("WorkBuddy ACP 目标定位失败（会话 {}）：{e}", args.sid),
                elapsed(&started),
            )
        }
    };
    // 转写基线（回合**前**读一次：佐证判「推进」）
    let baseline = transcript_mtime_ms(deps.home.as_deref(), &args.project, &target.session_id);
    let out = match client.prompt_on(&sess, &target, &args.payload) {
        Ok(o) => o,
        Err(e) => {
            return failed_outcome(
                &args.sid,
                &discovery,
                format!("WorkBuddy ACP 投递失败（会话 {}）：{e}", target.session_id),
                elapsed(&started),
            )
        }
    };
    let verdict = judge_prompt(&out);
    let corr = corroborate(
        deps.home.as_deref(),
        &args.project,
        &target.session_id,
        &args.payload,
        baseline,
    );
    let receipt = receipt_for(
        &args.sid,
        &verdict,
        &out,
        target.load.as_ref(),
        &corr,
        elapsed(&started),
    );
    WbTurnOutcome {
        receipt,
        endpoint: Some(discovery),
        verdict: Some(verdict),
        corroboration: corr,
    }
}

/// 生产/测试装配（**测试构建零网络零真实进程表**：缝全部来自 [`test_hooks`]——
/// 默认探针「无心跳/无 WB 进程」、默认 HTTP「任何请求传输失败」，故端点用例只能走到
/// 如实的「端点不可用」拒绝，**绝不落真网络**）。
///
/// 生产心跳读口 = `monitor::workbuddy_parser::heartbeat_snapshot`（`home` 为 None 时恒 None
/// ——连心跳文件都不读）；端口指纹只在 Windows 走（探测定案的平台差异）。
pub fn deps_for(
    os: &'static str,
    home: Option<PathBuf>,
    watchdog_ms: u64,
    sem: GlobalSem,
) -> WbDeps {
    #[cfg(test)]
    {
        WbDeps {
            sem,
            watchdog_ms,
            probe: test_hooks::probe(os),
            http: test_hooks::http(),
            home,
        }
    }
    #[cfg(not(test))]
    {
        let hb_home = home.clone();
        WbDeps {
            sem,
            watchdog_ms,
            probe: EndpointProbe {
                os,
                heartbeat: Box::new(move |pid| {
                    hb_home.as_deref().and_then(|h| {
                        crate::monitor::workbuddy_parser::heartbeat_snapshot(h, pid, now_ms())
                    })
                }),
                wb_pids: Box::new(production_wb_pids),
                listening_ports: Box::new(production_listening_ports),
            },
            http: production_http_seam(),
            home,
        }
    }
}

// ============================================================
// 流扫描器（纯核：喂块 + 现算时钟）——生产缝读取循环与单测共用
// ============================================================

/// SSE 流扫描器：**判定何时停**（响应帧 / 静默死线 / watchdog 上限）。
///
/// 「数据帧」判据 = 块里出现 `data:` 载荷行——**注释帧 `: heartbeat` 不算**（探测定案里
/// 「已结束会话」的流正是「200 + 心跳但回合不推进」，若把心跳当活性就会永远等下去）。
pub struct StreamScanner {
    policy: StreamPolicy,
    body: String,
    seen_needle: bool,
    started_ms: u64,
    last_data_ms: u64,
    stop: Option<StreamStop>,
}

impl StreamScanner {
    pub fn new(policy: StreamPolicy, now_ms: u64) -> Self {
        Self {
            policy,
            body: String::new(),
            seen_needle: false,
            started_ms: now_ms,
            last_data_ms: now_ms,
            stop: None,
        }
    }

    /// 喂入一块（返回 `true` = 已达停止条件）
    pub fn push(&mut self, chunk: &str, now_ms: u64) -> bool {
        self.body.push_str(chunk);
        if chunk.contains("data:") {
            self.last_data_ms = now_ms;
        }
        if let Some(needle) = self.policy.idle_unless_seen {
            if !self.seen_needle && self.body.contains(needle) {
                self.seen_needle = true;
            }
        }
        if let Some(id) = self.policy.stop_on_id {
            if response_frame_present(&self.body, id) {
                self.stop = Some(StreamStop::Complete);
            }
        }
        self.check_deadlines(now_ms)
    }

    /// 时钟推进（无数据的等待期；返回 `true` = 已达停止条件）
    pub fn tick(&mut self, now_ms: u64) -> bool {
        self.check_deadlines(now_ms)
    }

    fn check_deadlines(&mut self, now_ms: u64) -> bool {
        if self.stop.is_some() {
            return true;
        }
        // 静默死线：仅在「从未见到 idle_unless_seen」时提前停（已见 ⇒ 回合在跑，
        // 静默只受 max_ms 约束）
        let idle_applies = self.policy.idle_unless_seen.is_none() || !self.seen_needle;
        if self.policy.idle_ms > 0
            && idle_applies
            && now_ms.saturating_sub(self.last_data_ms) >= self.policy.idle_ms
        {
            self.stop = Some(StreamStop::Idle);
            return true;
        }
        if now_ms.saturating_sub(self.started_ms) >= self.policy.max_ms {
            self.stop = Some(StreamStop::Watchdog);
            return true;
        }
        false
    }

    /// 距下一个死线的剩余预算（毫秒；生产缝的 `timeout` 用它——**永不返回 0**）
    pub fn next_budget_ms(&self, now_ms: u64) -> u64 {
        let mut budget = self
            .policy
            .max_ms
            .saturating_sub(now_ms.saturating_sub(self.started_ms));
        let idle_applies = self.policy.idle_unless_seen.is_none() || !self.seen_needle;
        if self.policy.idle_ms > 0 && idle_applies {
            budget = budget.min(
                self.policy
                    .idle_ms
                    .saturating_sub(now_ms.saturating_sub(self.last_data_ms)),
            );
        }
        budget.max(1)
    }

    pub fn stop(&self) -> StreamStop {
        self.stop.unwrap_or(StreamStop::Complete)
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

/// 响应帧是否已在（`"id":<id>` 且同帧带 `result`/`error`）
fn response_frame_present(body: &str, id: u64) -> bool {
    parse_frames(body).0.iter().any(|f| {
        f.get("id").and_then(|x| x.as_u64()) == Some(id)
            && (f.get("result").is_some() || f.get("error").is_some())
    })
}

#[cfg(not(test))]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 生产 HTTP 缝（reqwest：rustls + system-proxy，与 `remote::tunnel` 同款客户机构建）。
///
/// **`cfg(not(test))`**：测试构建里本函数不存在 ⇒ 单测无法触达真实网络（宪法级纪律）。
/// **同步外壳**（house style）：调用方在 `spawn_blocking` 线程上跑，故此处
/// `tauri::async_runtime::block_on` 单次执行一条请求；流式响应用 [`StreamScanner`]
/// 逐块判停（`Response::chunk` 不需要 reqwest 的 `stream` feature——**不新增依赖**）。
#[cfg(not(test))]
pub fn production_http_seam() -> HttpSeam {
    Arc::new(|req: HttpReq| {
        let client = match reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(
                req.timeout_ms.clamp(500, 15_000),
            ))
            .timeout(std::time::Duration::from_millis(req.timeout_ms.max(1_000)))
            .build()
        {
            Ok(c) => c,
            Err(e) => return HttpResp::transport_error(&format!("HTTP 客户端构建失败: {e}")),
        };
        tauri::async_runtime::block_on(async move {
            let mut rb = client.request(
                reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET),
                &req.url,
            );
            for (k, v) in &req.headers {
                rb = rb.header(k.as_str(), v.as_str());
            }
            if let Some(body) = &req.body {
                rb = rb.body(body.clone());
            }
            let mut resp = match rb.send().await {
                Ok(r) => r,
                Err(e) => return HttpResp::transport_error(&format!("{e}")),
            };
            let status = resp.status().as_u16();
            if req.stream.idle_ms == 0 && req.stream.stop_on_id.is_none() {
                return match resp.text().await {
                    Ok(body) => HttpResp {
                        status,
                        body,
                        stop: StreamStop::Complete,
                        error: None,
                    },
                    Err(e) => HttpResp::transport_error(&format!("读响应体失败: {e}")),
                };
            }
            let mut scanner = StreamScanner::new(req.stream, now_ms());
            loop {
                let budget = scanner.next_budget_ms(now_ms());
                match tokio::time::timeout(std::time::Duration::from_millis(budget), resp.chunk())
                    .await
                {
                    // 静默/上限死线到点：如实标注停因（**不冒充收流完整**）
                    Err(_) => {
                        scanner.tick(now_ms().saturating_add(budget));
                        break;
                    }
                    Ok(Ok(None)) => break, // 流自然结束
                    Ok(Ok(Some(chunk))) => {
                        if scanner.push(&String::from_utf8_lossy(&chunk), now_ms()) {
                            break;
                        }
                    }
                    Ok(Err(e)) => return HttpResp::transport_error(&format!("流读取失败: {e}")),
                }
            }
            HttpResp {
                status,
                body: scanner.body().to_string(),
                stop: scanner.stop(),
                error: None,
            }
        })
    })
}

/// WB 宿主进程表（生产；sysinfo 只刷新 exe 字段——**只服务端口发现**）
#[cfg(not(test))]
fn production_wb_pids() -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    sys.processes()
        .values()
        .filter(|p| is_wb_host_exe(p.exe()))
        .map(|p| p.pid().as_u32())
        .collect()
}

/// pid 的监听端口表（生产；`Get-NetTCPConnection -State Listen` 按 pid 过滤——
/// **不扫全端口空间**，只问 WB 自己监听了哪些）
#[cfg(not(test))]
fn production_listening_ports(pid: u32) -> Vec<u16> {
    if pid == 0 {
        return Vec::new();
    }
    let script = format!(
        "Get-NetTCPConnection -State Listen -OwningProcess {pid} -ErrorAction SilentlyContinue \
         | Select-Object -ExpandProperty LocalPort"
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) => parse_ports(&String::from_utf8_lossy(&o.stdout)),
        Err(e) => {
            log::warn!("workbuddy: Get-NetTCPConnection 启动失败: {e}");
            Vec::new()
        }
    }
}

// ============================================================
// 测试专用注入钩子（仅 cfg(test)）
// ============================================================

/// **测试专用注入钩子**（仅 `cfg(test)`）：端点级分派链在测试构建里本应恒「端点不可用」，
/// 覆盖不到「真 ACP 链 → 回执 → 审计 → 锁注销」。故给端点用例三个可控面（心跳 / WB 进程 /
/// HTTP 缝），**生产构建里这些开关根本不存在**。
///
/// **并行纪律**（钩子是进程级全局态）：凡读写钩子的用例必须持 [`test_hooks::LOCK`]。
#[cfg(test)]
pub mod test_hooks {
    use super::*;
    use std::sync::Mutex;

    /// 钩子用例串行锁（tokio 互斥量：守卫跨 await 持有，std 守卫会被
    /// `clippy::await_holding_lock` 正当拦下——与 codex `test_hooks::LOCK` 同款）
    type HookLock = std::sync::LazyLock<tokio::sync::Mutex<()>>;
    pub(crate) static LOCK: HookLock = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    static HEARTBEAT: std::sync::LazyLock<Mutex<Option<HeartbeatSnapshot>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    static PIDS: std::sync::LazyLock<Mutex<Vec<u32>>> =
        std::sync::LazyLock::new(|| Mutex::new(Vec::new()));
    /// pid → 监听端口表（类型别名避免 clippy::type_complexity）
    type HookPorts = std::sync::LazyLock<Mutex<Vec<(u32, Vec<u16>)>>>;
    static PORTS: HookPorts = std::sync::LazyLock::new(|| Mutex::new(Vec::new()));
    /// 装填的 HTTP 缝槽位（类型别名避免 clippy::type_complexity）
    type HookHttp = std::sync::LazyLock<Mutex<Option<HttpSeam>>>;
    static HTTP: HookHttp = std::sync::LazyLock::new(|| Mutex::new(None));

    /// 心跳钩子形态
    #[derive(Debug, Clone)]
    pub struct FakeHeartbeat {
        pub session_id: String,
        pub endpoint: Option<String>,
        pub usable: bool,
    }

    /// 装填钩子（**调用方必须已持 [`LOCK`]**）
    pub(crate) fn set(hb: Option<FakeHeartbeat>, pids: Vec<u32>, ports: Vec<(u32, Vec<u16>)>) {
        *HEARTBEAT.lock().unwrap_or_else(|e| e.into_inner()) = hb.map(|h| HeartbeatSnapshot {
            session_id: h.session_id,
            endpoint: h.endpoint,
            usable: h.usable,
        });
        *PIDS.lock().unwrap_or_else(|e| e.into_inner()) = pids;
        *PORTS.lock().unwrap_or_else(|e| e.into_inner()) = ports;
    }

    /// 装填 HTTP 缝（mock 的共享句柄留在用例侧供断言）
    pub(crate) fn set_http(seam: HttpSeam) {
        *HTTP.lock().unwrap_or_else(|e| e.into_inner()) = Some(seam);
    }

    /// 清空钩子（用例收尾；Drop 守卫调用）
    pub(crate) fn clear() {
        *HEARTBEAT.lock().unwrap_or_else(|e| e.into_inner()) = None;
        PIDS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        PORTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *HTTP.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 探针缝（默认：无心跳、无 WB 进程、无端口——测试构建如实「端点不可用」）
    pub(crate) fn probe(os: &'static str) -> EndpointProbe {
        EndpointProbe {
            os,
            heartbeat: Box::new(|_pid| HEARTBEAT.lock().unwrap_or_else(|e| e.into_inner()).clone()),
            wb_pids: Box::new(|| PIDS.lock().unwrap_or_else(|e| e.into_inner()).clone()),
            listening_ports: Box::new(|pid| {
                PORTS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|(p, _)| *p == pid)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            }),
        }
    }

    /// HTTP 缝（默认：**任何请求一律传输失败**——测试构建绝不落真网络）
    pub(crate) fn http() -> HttpSeam {
        if let Some(seam) = HTTP.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return seam;
        }
        Arc::new(|_req| HttpResp::transport_error("测试构建未装填 HTTP 缝（零网络纪律）"))
    }
}

// ============================================================
// MockHttp（注入缝的测试实现；**测试唯一 IO 路径**）
// ============================================================

/// ACP wire 的注入缝桩：**有序期望表** + 请求留痕 + 违规留痕。
///
/// - 期望**按序消费**（FIFO）：method/path/头逐项校验；不匹配即记违规并返回传输失败
///   （**测试必然红**，不会静默放过）；
/// - 未预期的请求同样记违规（证明「除了这条缝没有别的路」——没有真实客户端兜底）；
/// - [`MockHttp::assert_clean`] = 期望全消费 + 零违规（用例收尾铁律）。
#[cfg(test)]
#[derive(Clone, Default)]
pub struct MockHttp {
    inner: Arc<MockInner>,
}

#[cfg(test)]
#[derive(Default)]
struct MockInner {
    expects: std::sync::Mutex<std::collections::VecDeque<Expectation>>,
    calls: std::sync::Mutex<Vec<HttpReq>>,
    violations: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
struct Expectation {
    method: &'static str,
    path: String,
    headers: Vec<String>,
    resp: HttpResp,
}

#[cfg(test)]
pub struct PendingExpect<'a> {
    mock: &'a mut MockHttp,
    exp: Expectation,
}

#[cfg(test)]
impl MockHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn expect_post(&mut self, path: &str, headers: Option<&[&str]>) -> PendingExpect<'_> {
        PendingExpect {
            mock: self,
            exp: Expectation {
                method: "POST",
                path: path.to_string(),
                headers: headers
                    .map(|h| h.iter().map(|s| (*s).to_string()).collect())
                    .unwrap_or_default(),
                resp: HttpResp::transport_error("期望未装填响应"),
            },
        }
    }

    pub fn expect_get(&mut self, path: &str) -> PendingExpect<'_> {
        PendingExpect {
            mock: self,
            exp: Expectation {
                method: "GET",
                path: path.to_string(),
                headers: Vec::new(),
                resp: HttpResp::transport_error("期望未装填响应"),
            },
        }
    }

    /// 请求留痕（顺序 = 实际发生顺序）
    pub fn calls(&self) -> Vec<HttpReq> {
        self.inner
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn violations(&self) -> Vec<String> {
        self.inner
            .violations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn expectations_left(&self) -> usize {
        self.inner
            .expects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// 用例收尾铁律：期望全消费 + 零违规
    pub fn assert_clean(&self) {
        assert!(
            self.violations().is_empty(),
            "MockHttp 违规（未预期请求/头不符）：{:?}\n实际请求：{:?}",
            self.violations(),
            self.calls()
        );
        assert_eq!(
            self.expectations_left(),
            0,
            "MockHttp 期望未全部消费（wire 序列与预期不符）：{:?}",
            self.calls()
        );
    }

    /// 交给 `AcpClient` 的缝（与 mock 共享同一份账本）
    pub fn seam(self) -> HttpSeam {
        let inner = self.inner.clone();
        Arc::new(move |req: HttpReq| Self::handle(&inner, req))
    }

    fn handle(inner: &Arc<MockInner>, req: HttpReq) -> HttpResp {
        inner
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(req.clone());
        let next = inner
            .expects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        let Some(exp) = next else {
            inner
                .violations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("未预期的请求：{} {}", req.method, req.url));
            return HttpResp::transport_error("MockHttp：未预期的请求（测试零网络纪律）");
        };
        if let Some(why) = expectation_mismatch(&exp, &req) {
            inner
                .violations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(why);
            return HttpResp::transport_error("MockHttp：期望不匹配（测试零网络纪律）");
        }
        exp.resp.clone()
    }
}

#[cfg(test)]
impl From<MockHttp> for HttpSeam {
    /// 直接 `AcpClient::new(base, mock)`（plan Step 1 原例的调用形状）
    fn from(mock: MockHttp) -> Self {
        mock.seam()
    }
}

#[cfg(test)]
impl PendingExpect<'_> {
    fn push(self, resp: HttpResp) {
        self.mock
            .inner
            .expects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(Expectation { resp, ..self.exp });
    }

    pub fn respond_json(self, body: &str) {
        self.push(HttpResp::json(body));
    }

    pub fn respond_sse(self, body: &str) {
        self.push(HttpResp::sse(body));
    }

    /// 静默死线掐断的 SSE（探测定案：已结束会话形态）
    pub fn respond_stalled_sse(self, body: &str) {
        self.push(HttpResp::sse_stalled(body));
    }

    /// watchdog 上限掐断的 SSE
    pub fn respond_watchdog_sse(self, body: &str) {
        self.push(HttpResp::sse_watchdog(body));
    }

    pub fn respond_status(self, status: u16, body: &str) {
        self.push(HttpResp::status(status, body));
    }

    pub fn respond_transport_error(self, msg: &str) {
        self.push(HttpResp::transport_error(msg));
    }
}

/// 期望 vs 实际（返回 `Some(说明)` = 不符）：路径取 URL 的 path 段；头名大小写不敏感、
/// 值逐字；头规格 `name` = 只要求在场，`name: value` = 值须相等
#[cfg(test)]
fn expectation_mismatch(exp: &Expectation, req: &HttpReq) -> Option<String> {
    if exp.method != req.method {
        return Some(format!("方法不符：期望 {} 实际 {}", exp.method, req.method));
    }
    let path = req
        .url
        .split("//")
        .nth(1)
        .and_then(|s| s.find('/').map(|i| &s[i..]));
    let path = path.unwrap_or("/");
    if path != exp.path {
        return Some(format!(
            "路径不符：期望 {} 实际 {}（{}）",
            exp.path, path, req.url
        ));
    }
    for spec in &exp.headers {
        let (name, want) = match spec.split_once(':') {
            Some((n, v)) => (n.trim().to_lowercase(), Some(v.trim().to_string())),
            None => (spec.trim().to_lowercase(), None),
        };
        let got = req
            .headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == name)
            .map(|(_, v)| v.clone());
        match (got, want) {
            (None, _) => return Some(format!("缺头：{name}（实际头：{:?}）", req.headers)),
            (Some(_), None) => {}
            (Some(g), Some(w)) if g != w => {
                return Some(format!("头值不符：{name} 期望 {w} 实际 {g}"))
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ACP wire 头三件套（plan Step 1 原例逐字；头名大小写不敏感、值逐字）
    const ACP_HDRS: &[&str] = &[
        "acp-connection-id",
        "acp-session-token",
        "Accept: application/json, text/event-stream",
    ];
    const CID: &str = "c1";
    const TOK: &str = "t1";
    const SID: &str = "3f12ca20-eae5-4713-a7a4-adf64a44f346";
    const CWD: &str = "E:/t2";
    const BASE: &str = "http://127.0.0.1:63928";

    /// SSE 帧构造（真实 wire：`:ok` 注释 + `event:`/`data:` 帧）
    fn sse(frames: &[serde_json::Value]) -> String {
        let mut s = String::from(":ok\n\n");
        for f in frames {
            s.push_str("event: message\ndata: ");
            s.push_str(&f.to_string());
            s.push_str("\n\n");
        }
        s
    }

    fn rpc_ok(id: u64, result: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
    }

    fn rpc_update(params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"jsonrpc": "2.0", "method": "session/update", "params": params})
    }

    fn phase_frame(sid: &str, phase: &str) -> serde_json::Value {
        rpc_update(serde_json::json!({
            "sessionId": sid,
            "update": {"sessionUpdate": "agent_phase", "agentPhase": phase},
        }))
    }

    fn init_sse(id: u64) -> String {
        sse(&[rpc_ok(
            id,
            serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": true, "promptCapabilities": {"image": true}},
            }),
        )])
    }

    fn load_sse(id: u64, last_stop: Option<&str>, mode: &str) -> String {
        let mut frames = vec![
            rpc_update(serde_json::json!({
                "sessionId": SID,
                "update": {"sessionUpdate": "config_option_update", "id": "mode",
                           "name": "Permission Mode", "category": "mode",
                           "currentValue": mode,
                           "options": [{"value": "default", "name": "Always Ask"},
                                       {"value": "acceptEdits", "name": "Accept Edits"},
                                       {"value": "plan", "name": "Plan"},
                                       {"value": "auto", "name": "Auto"}]},
            })),
            rpc_ok(id, serde_json::json!({})),
        ];
        if let Some(stop) = last_stop {
            frames.insert(
                0,
                rpc_update(serde_json::json!({
                    "sessionId": SID,
                    "update": {"sessionUpdate": "session_end", "stopReason": stop},
                })),
            );
        }
        sse(&frames)
    }

    fn new_sse(id: u64, new_sid: &str) -> String {
        sse(&[rpc_ok(id, serde_json::json!({ "sessionId": new_sid }))])
    }

    fn prompt_sse(id: u64, phases: &[&str], stop: Option<&str>, assistant: Option<&str>) -> String {
        let mut frames = Vec::new();
        for p in phases {
            frames.push(phase_frame(SID, p));
        }
        if let Some(a) = assistant {
            frames.push(rpc_update(serde_json::json!({
                "sessionId": SID,
                "update": {"sessionUpdate": "agent_message_chunk",
                           "content": {"type": "text", "text": a}},
            })));
        }
        if let Some(s) = stop {
            frames.push(rpc_ok(id, serde_json::json!({ "stopReason": s })));
        }
        sse(&frames)
    }

    /// 生产缝的最小装配（测试自持名额：不抢全局 `GlobalSem`）
    fn deps(
        http: MockHttp,
        hb: Option<test_hooks::FakeHeartbeat>,
        pids: Vec<u32>,
        ports: Vec<(u32, Vec<u16>)>,
    ) -> WbDeps {
        let hb = hb.map(|h| HeartbeatSnapshot {
            session_id: h.session_id,
            endpoint: h.endpoint,
            usable: h.usable,
        });
        WbDeps {
            sem: GlobalSem::new(4),
            watchdog_ms: 600_000,
            probe: EndpointProbe {
                os: "windows",
                heartbeat: Box::new(move |_pid| hb.clone()),
                wb_pids: Box::new(move || pids.clone()),
                listening_ports: Box::new(move |pid| {
                    ports
                        .iter()
                        .find(|(p, _)| *p == pid)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                }),
            },
            http: http.seam(),
            home: None,
        }
    }

    /// 钩子收尾守卫（panic 也清——钩子是进程级全局态，别把假端点留给后续用例）
    struct HookClear;
    impl Drop for HookClear {
        fn drop(&mut self) {
            test_hooks::clear();
        }
    }

    fn args() -> WbTurnArgs {
        WbTurnArgs {
            sid: SID.to_string(),
            project: CWD.to_string(),
            payload: "probe [mobile iPad]".to_string(),
            pid: 77765,
        }
    }

    fn live_heartbeat() -> test_hooks::FakeHeartbeat {
        test_hooks::FakeHeartbeat {
            session_id: SID.to_string(),
            endpoint: Some(BASE.to_string()),
            usable: true,
        }
    }

    // ===== plan Step 1 原例（Task 11 的红灯测试） =====

    /// plan Step 1 原例①（wire 以 Mac 实测为准）：connect（免鉴权）→ initialize →
    /// 二分（新建 = `session/new`）→ `session/prompt`；`sess.token == "t1"`、
    /// `ev.first_phase == "model_requesting"`。
    /// **与 plan 草图的一处偏差（如实登记）**：草图把整条 ACP 链压成**一次** `POST
    /// /api/v1/acp`，而实测 wire 是**逐请求一次 POST**（initialize / session/new /
    /// session/prompt 各一次），故期望表按真实序列装填四次——草图是提纲不是 wire。
    #[test]
    fn acp_handshake_and_prompt_sequence() {
        let mut http = MockHttp::new();
        http.expect_post(CONNECT_PATH, None)
            .respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&init_sse(1));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&new_sse(2, "s-new-1"));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&prompt_sse(
                3,
                &["model_requesting"],
                Some("end_turn"),
                Some("好的"),
            ));
        let client = AcpClient::new(BASE, http.clone());
        let sess = client.handshake().unwrap();
        assert_eq!(sess.token, TOK);
        assert_eq!(sess.connection_id, CID);
        let ev = client
            .prompt(&sess, "probe [mobile]", SessionSel::New { cwd: CWD })
            .unwrap();
        assert_eq!(ev.first_phase, "model_requesting");
        assert_eq!(ev.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(ev.last_assistant.as_deref(), Some("好的"));
        assert_eq!(
            ev.session_id, "s-new-1",
            "新建路的会话号 = session/new 产物"
        );
        http.assert_clean();
    }

    /// plan Step 1 原例②（探测定案）：load 已结束会话 → prompt 只「接收但未执行」
    /// （200 + 心跳、回合不推进）⇒ **不许静默挂**：60s 无 `agentPhase` 即判「已结束需复活」
    /// → 回执 `failed(stage=ChannelError, reason=「会话已结束（end_turn），需复活语义…」)`
    #[test]
    fn ended_session_prompt_reports_revival_needed() {
        let mut http = MockHttp::new();
        http.expect_post(CONNECT_PATH, None)
            .respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&init_sse(1));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&load_sse(2, Some("end_turn"), "fullAccess"));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_stalled_sse(":ok\n\n: heartbeat\n");
        let fx = deps(http.clone(), Some(live_heartbeat()), Vec::new(), Vec::new());
        let out = run_turn(&args(), &fx);
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(
            out.receipt.stage,
            Some(Stage::ChannelError),
            "通道跑过了但回合没推进：不能报 ok，也不是投递前拒绝：{:?}",
            out.receipt
        );
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(reason.contains("会话已结束"), "{reason}");
        assert!(reason.contains("需复活"), "{reason}");
        assert!(
            reason.contains("重开该会话") || reason.contains("走新建"),
            "必须引导用户重开或新建：{reason}"
        );
        assert!(
            reason.contains("60s") && reason.contains("agentPhase"),
            "必须说清 60s 无 agentPhase 的判据：{reason}"
        );
        assert!(
            reason.contains("end_turn"),
            "load 回放的 end_turn 是诊断证据：{reason}"
        );
        assert_eq!(out.receipt.session_id, SID);
        http.assert_clean();
    }

    // ===== 端点发现 =====

    /// Win 5.7.3 未启用远程控制（无心跳、无 WB 进程）：**直接如实拒绝**——文案带启用条件，
    /// **零 HTTP 请求**（不盲扫），且是投递前拒绝（零字节投递）
    #[test]
    fn endpoint_absent_is_an_honest_refusal_naming_the_enablement_condition() {
        let http = MockHttp::new(); // 零期望：任何请求都是违规
        let fx = deps(http.clone(), None, Vec::new(), Vec::new());
        let out = run_turn(&args(), &fx);
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(
            out.receipt.stage,
            Some(Stage::Refused),
            "未起跑、零字节投递 = 投递前拒绝（不是 channel_error）：{:?}",
            out.receipt
        );
        assert_eq!(
            out.receipt.terminator(),
            super::super::receipt::Terminator::NotStarted
        );
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("WorkBuddy 远程控制端点未启用"),
            "plan Step 3 逐字文案：{reason}"
        );
        assert!(
            reason.contains("请在 WorkBuddy 设置中开启远程控制"),
            "{reason}"
        );
        assert!(reason.contains("风险 16"), "{reason}");
        assert_eq!(http.calls().len(), 0, "无端点不盲扫：零 HTTP 请求");
        http.assert_clean();
    }

    /// 心跳端点路（macOS/旧版形态）：会话号匹配 + 可用 + 回环 → 找到；
    /// 会话号不匹配 / 心跳过期（usable=false）/ 非回环 → 一律不用（不猜、不外发）
    #[test]
    fn heartbeat_endpoint_requires_session_match_freshness_and_loopback() {
        let probe = |hb: Option<HeartbeatSnapshot>| EndpointProbe {
            os: "macos",
            heartbeat: Box::new(move |_pid| hb.clone()),
            wb_pids: Box::new(Vec::<u32>::new),
            listening_ports: Box::new(|_pid| Vec::<u16>::new()),
        };
        let http = MockHttp::new().seam();
        let facts = |sid: &str, ep: Option<&str>, usable: bool| HeartbeatSnapshot {
            session_id: sid.to_string(),
            endpoint: ep.map(str::to_string),
            usable,
        };
        assert_eq!(
            discover_endpoint(
                &probe(Some(facts(SID, Some(BASE), true))),
                &http,
                SID,
                77765
            ),
            EndpointDiscovery::Found {
                url: BASE.to_string(),
                source: EndpointSource::Heartbeat { pid: 77765 }
            }
        );
        for bad in [
            facts("other-session", Some(BASE), true), // 别的会话的心跳
            facts(SID, Some(BASE), false),            // 过期（私有格式的陈旧心跳）
            facts(SID, None, true),                   // 无 endpoint 字段
            facts(SID, Some("http://10.0.0.9:1"), true), // 非回环（绝不出本机）
        ] {
            match discover_endpoint(&probe(Some(bad.clone())), &http, SID, 77765) {
                EndpointDiscovery::Unavailable { reason } => {
                    assert!(
                        reason.contains("WorkBuddy 远程控制端点未启用"),
                        "不可用原因必须带启用条件：{reason}"
                    );
                }
                other => panic!("该形态不得判为可用：{bad:?} → {other:?}"),
            }
        }
    }

    /// Windows 端口指纹路：**双确认**（`/` 标题指纹 **且** `/health` UP）才认；
    /// 单侧命中一律不认（防把别的本地服务当 WB 远程控制端点）
    #[test]
    fn port_fingerprint_requires_both_confirmations() {
        assert!(fingerprint_hit(
            "<title>CodeBuddy Remote Control</title>",
            r#"{"status":"UP"}"#
        ));
        assert!(
            !fingerprint_hit(
                "<title>CodeBuddy Remote Control</title>",
                r#"{"status":"DOWN"}"#
            ),
            "title 命中但 health 不对 ⇒ 不认"
        );
        assert!(
            !fingerprint_hit("<title>something else</title>", r#"{"status":"UP"}"#),
            "health 对但 title 不对 ⇒ 不认"
        );
        assert!(!health_ok("not json"), "非 JSON 不猜");
        assert!(
            health_ok(" {\"status\":\"up\"} \n"),
            "空白/大小写容忍（JSON 语义不变）"
        );
        // 端口表解析：表头/噪声/越界一律跳过
        assert_eq!(
            parse_ports("LocalPort\r\n35578\r\n\r\n63928\r\nnot-a-port\r\n70000\r\n0\r\n"),
            vec![35578, 63928],
            "只收合法端口：噪声与越界值不猜"
        );
        // 宿主判定（只服务端口发现）
        assert!(is_wb_host_exe(Some(Path::new("C:/x/WorkBuddy.exe"))));
        assert!(!is_wb_host_exe(Some(Path::new("C:/x/codex.exe"))));
        assert!(!is_wb_host_exe(None));
        // 非回环一律丢弃
        assert_eq!(
            loopback_url("http://127.0.0.1:63928"),
            Some(BASE.to_string())
        );
        assert_eq!(
            loopback_url("http://localhost:1/"),
            Some("http://localhost:1".into())
        );
        assert_eq!(loopback_url("http://10.0.0.9:63928"), None);
        assert_eq!(loopback_url("file:///etc/passwd"), None);
    }

    /// 端口指纹路的完整发现（WB 进程在、端口双确认）：经**同一条** HTTP 缝拿 `/` 与 `/health`
    #[test]
    fn port_fingerprint_discovery_uses_the_injected_seam() {
        let mut http = MockHttp::new();
        http.expect_get("/")
            .respond_json("<title>CodeBuddy Remote Control</title>");
        http.expect_get("/health")
            .respond_json(r#"{"status":"UP"}"#);
        let probe = EndpointProbe {
            os: "windows",
            heartbeat: Box::new(|_pid| None),
            wb_pids: Box::new(|| vec![4242]),
            listening_ports: Box::new(|_pid| vec![63928]),
        };
        let found = discover_endpoint(&probe, &http.clone().seam(), SID, 0);
        assert_eq!(
            found,
            EndpointDiscovery::Found {
                url: BASE.to_string(),
                source: EndpointSource::PortFingerprint {
                    pid: 4242,
                    port: 63928
                }
            }
        );
        http.assert_clean();
    }

    // ===== SSE 归一 =====

    /// SSE 归一：注释帧不算推进证据；未知帧只计数不解释；phase/assistant/mode/stop 归位
    #[test]
    fn sse_shapes_normalize_without_guessing() {
        let body = format!(
            ":ok\n\nevent: message\ndata: {}\n\n: heartbeat\n\ndata: {{\"whatever\":1}}\n\ndata: not-json\n\ndata: {}\n\n",
            phase_frame(SID, "model_requesting"),
            rpc_ok(3, serde_json::json!({"stopReason": "end_turn"}))
        );
        let ev = read_stream(&body);
        assert_eq!(ev.first_phase(), "model_requesting");
        assert!(ev.advanced());
        assert_eq!(ev.frames, 2, "只有含已知键的帧计数（phase + 终点）");
        assert_eq!(ev.comments, 2, "`:ok` 与 `: heartbeat` 都是注释帧");
        assert_eq!(
            ev.unknown_frames, 1,
            "JSON 但无已知键 ⇒ 只计数不解释（Task 6 同纪律）"
        );
        assert_eq!(ev.noise_lines, 1, "既非注释也非 JSON ⇒ 噪音行");
        assert_eq!(ev.stop_reason.as_deref(), Some("end_turn"));
        // RPC 错误帧
        let err = read_stream(&format!(
            "data: {}\n\n",
            serde_json::json!({"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"Missing acp-connection-id header"}})
        ));
        assert_eq!(
            err.rpc_error.as_deref(),
            Some("Missing acp-connection-id header")
        );
        // load 回放：session_end + mode 选项
        let load = read_stream(&load_sse(2, Some("end_turn"), "fullAccess"));
        assert!(load.session_end);
        assert_eq!(load.permission_mode.as_deref(), Some("fullAccess"));
        assert_eq!(load.stop_reason.as_deref(), Some("end_turn"));
    }

    /// 流扫描器钉死**诚实性的时间判据**：心跳（注释帧）不重置静默计时 ⇒ 60s 无 `agentPhase`
    /// 即停（[`StreamStop::Idle`]）；已见 `agentPhase` 后静默不再提前停；响应帧即停；
    /// watchdog 上限照样掐。
    #[test]
    fn stream_scanner_pins_the_phase_deadline() {
        let policy = StreamPolicy {
            max_ms: 600_000,
            idle_ms: PHASE_DEADLINE_MS,
            idle_unless_seen: Some("agentPhase"),
            stop_on_id: Some(3),
        };
        // ① 只有心跳：静默计时不重置
        let mut sc = StreamScanner::new(policy, 0);
        assert!(!sc.push(":ok\n\n: heartbeat\n", 1_000));
        assert!(!sc.push(": heartbeat\n", 59_000));
        assert!(
            sc.push(": heartbeat\n", PHASE_DEADLINE_MS + 1),
            "60s 无 data 帧且从未见 agentPhase ⇒ 必须停（不许静默挂）"
        );
        assert_eq!(sc.stop(), StreamStop::Idle);
        // ② 已见 agentPhase：静默只受 max_ms 约束（回合真的在跑）
        let mut sc2 = StreamScanner::new(policy, 0);
        assert!(!sc2.push("data: {\"agentPhase\":\"model_requesting\"}\n\n", 10));
        assert!(!sc2.tick(120_000), "已见 agentPhase ⇒ 不得按 60s 提前停");
        // ③ 响应帧即停
        let mut sc3 = StreamScanner::new(policy, 0);
        assert!(sc3.push(
            "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}\n\n",
            10
        ));
        assert_eq!(sc3.stop(), StreamStop::Complete);
        // ④ watchdog 上限（静默死线关掉）
        let mut sc4 = StreamScanner::new(
            StreamPolicy {
                max_ms: 100,
                idle_ms: 0,
                ..policy
            },
            0,
        );
        assert!(sc4.tick(100));
        assert_eq!(sc4.stop(), StreamStop::Watchdog);
        // 预算永不返回 0（生产 timeout 会即刻到点）
        assert!(sc4.next_budget_ms(0) >= 1);
    }

    // ===== 判定与诚实性 =====

    /// 推进了但没拿到终点：**不冒充成功**（watchdog ⇒ timeout；流自己断了 ⇒ channel_error）
    #[test]
    fn advanced_without_terminal_is_never_reported_as_success() {
        let out = PromptOutcome {
            session_id: SID.into(),
            first_phase: "model_requesting".into(),
            phases: vec!["model_requesting".into()],
            stop: StreamStop::Watchdog,
            ..Default::default()
        };
        assert_eq!(
            judge_prompt(&out),
            PromptVerdict::Advanced {
                phase: "model_requesting".into(),
                stop: StreamStop::Watchdog
            }
        );
        let r = receipt_for(
            SID,
            &judge_prompt(&out),
            &out,
            None,
            &Corroboration::default(),
            7,
        );
        assert_eq!(r.stage, Some(Stage::Timeout), "watchdog 掐断 = timeout 档");
        assert!(!r.reason.clone().unwrap_or_default().is_empty());
        let out2 = PromptOutcome {
            stop: StreamStop::Idle,
            ..out.clone()
        };
        let r2 = receipt_for(
            SID,
            &judge_prompt(&out2),
            &out2,
            None,
            &Corroboration::default(),
            7,
        );
        assert_eq!(r2.stage, Some(Stage::ChannelError));
        assert!(
            r2.reason.clone().unwrap_or_default().contains("未拿到终点"),
            "如实说清「没拿到终点」：{:?}",
            r2.reason
        );
    }

    /// 传输错误 / RPC 错误 ⇒ 如实的 channel_error（带原始报文摘要，不吞错）
    #[test]
    fn transport_and_rpc_errors_are_reported_honestly() {
        let mut http = MockHttp::new();
        http.expect_post(CONNECT_PATH, None)
            .respond_transport_error("connect refused");
        let fx = deps(http.clone(), Some(live_heartbeat()), Vec::new(), Vec::new());
        let out = run_turn(&args(), &fx);
        assert_eq!(out.receipt.stage, Some(Stage::ChannelError));
        assert!(
            out.receipt
                .reason
                .clone()
                .unwrap_or_default()
                .contains("connect refused"),
            "必须带出传输层原因：{:?}",
            out.receipt.reason
        );
        http.assert_clean();
        // RPC 错误：load 被服务端拒（如会话不存在）——同样如实
        let mut http2 = MockHttp::new();
        http2
            .expect_post(CONNECT_PATH, None)
            .respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        http2
            .expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&init_sse(1));
        http2.expect_post(ACP_PATH, Some(ACP_HDRS)).respond_sse(&format!(
            "data: {}\n\n",
            serde_json::json!({"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"session not found"}})
        ));
        let fx2 = deps(
            http2.clone(),
            Some(live_heartbeat()),
            Vec::new(),
            Vec::new(),
        );
        let out2 = run_turn(&args(), &fx2);
        assert_eq!(out2.receipt.stage, Some(Stage::ChannelError));
        assert!(
            out2.receipt
                .reason
                .clone()
                .unwrap_or_default()
                .contains("session not found"),
            "必须带出服务端原因：{:?}",
            out2.receipt.reason
        );
        http2.assert_clean();
        // 建连非 2xx（HTTP 500）→ 如实带状态码，不猜体
        let mut http3 = MockHttp::new();
        http3
            .expect_post(CONNECT_PATH, None)
            .respond_status(500, "boom");
        let fx3 = deps(
            http3.clone(),
            Some(live_heartbeat()),
            Vec::new(),
            Vec::new(),
        );
        let out3 = run_turn(&args(), &fx3);
        assert_eq!(out3.receipt.stage, Some(Stage::ChannelError));
        assert!(
            out3.receipt
                .reason
                .clone()
                .unwrap_or_default()
                .contains("HTTP 500"),
            "必须带出状态码：{:?}",
            out3.receipt.reason
        );
        http3.assert_clean();
    }

    /// watchdog 掐断的**推进中**回合：回执 timeout（**不冒充成功**），并说清已见 agentPhase
    #[test]
    fn watchdog_cut_stream_maps_to_timeout_without_faking_success() {
        let mut http = MockHttp::new();
        http.expect_post(CONNECT_PATH, None)
            .respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&init_sse(1));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&load_sse(2, None, "fullAccess"));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_watchdog_sse(&format!(
                "data: {}\n\n",
                phase_frame(SID, "model_requesting")
            ));
        let fx = deps(http.clone(), Some(live_heartbeat()), Vec::new(), Vec::new());
        let out = run_turn(&args(), &fx);
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::Timeout), "{:?}", out.receipt);
        assert_eq!(
            out.receipt.terminator(),
            super::super::receipt::Terminator::Watchdog
        );
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("未拿到终点") && reason.contains("model_requesting"),
            "{reason}"
        );
        http.assert_clean();
    }

    /// 成功回执：末条 assistant 截断走 Task 6 单点；**转写佐证未命中时如实标注**
    /// （ok + reason 说明「佐证未命中」，绝不把未确认说成已确认）
    #[test]
    fn completed_receipt_reports_transcript_corroboration_state() {
        let mut http = MockHttp::new();
        http.expect_post(CONNECT_PATH, None)
            .respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&init_sse(1));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&load_sse(2, None, "fullAccess"));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&prompt_sse(
                3,
                &["model_requesting", "generating"],
                Some("end_turn"),
                Some(&"答".repeat(260)),
            ));
        let fx = deps(http.clone(), Some(live_heartbeat()), Vec::new(), Vec::new());
        let out = run_turn(&args(), &fx);
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        let last = out.receipt.last_assistant.clone().unwrap();
        assert_eq!(
            last.chars().count(),
            super::super::receipt::LAST_ASSISTANT_CHARS + 1,
            "截断口径必须复用 Task 6 单点（200 字 + 省略号）"
        );
        assert_eq!(out.receipt.tokens, None, "ACP 未给 token ⇒ 不编");
        assert!(
            out.receipt
                .reason
                .clone()
                .unwrap_or_default()
                .contains("转写佐证未命中"),
            "ok 回执必须如实标注佐证状态：{:?}",
            out.receipt.reason
        );
        http.assert_clean();
    }

    /// 转写佐证（tempdir 夹具，**零真实 ~/.workbuddy**）：mtime 推进 / 尾部命中载荷标记 /
    /// 文件缺席 三形态；复用读侧 `find_session_jsonl`（不另写转写解析）
    #[test]
    fn transcript_corroboration_is_second_evidence_only() {
        let home = tempfile::tempdir().unwrap();
        let dir = home
            .path()
            .join(".workbuddy")
            .join("projects")
            .join(crate::monitor::workbuddy_parser::mangle_project_path(CWD));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join(format!("{SID}.jsonl"));
        std::fs::write(
            &jsonl,
            "{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"text\":\"probe [mobile iPad]\"}]}\n",
        )
        .unwrap();
        let base = transcript_mtime_ms(Some(home.path()), CWD, SID);
        assert!(base.is_some(), "在场文件必须给出 mtime 基线");
        // 1) 不在场 → 未确认
        let absent = corroborate(
            Some(home.path()),
            CWD,
            "e3b0c442-0000-4000-8000-000000000000",
            "x",
            None,
        );
        assert!(!absent.present && !absent.confirmed());
        // 2) 在场 + 尾部命中标记 → 确认（mtime 未推进也认）
        let hit = corroborate(Some(home.path()), CWD, SID, "probe [mobile iPad]", base);
        assert!(hit.present && hit.needle_hit && hit.confirmed(), "{hit:?}");
        // 3) 在场 + mtime 推进 → 确认（主判据；用 filetime 显式置后——不靠写入竞速）
        std::fs::write(
            &jsonl,
            "{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}\n",
        )
        .unwrap();
        let later = base.unwrap() + 5_000;
        filetime::set_file_mtime(
            &jsonl,
            filetime::FileTime::from_unix_time(
                (later / 1000) as i64,
                ((later % 1000) * 1_000_000) as u32,
            ),
        )
        .unwrap();
        let adv = corroborate(Some(home.path()), CWD, SID, "nothing-matches", base);
        assert!(adv.present && adv.advanced && adv.confirmed(), "{adv:?}");
        // 标记取 W4 尾签名（可读、稳定）
        assert_eq!(corroboration_needle("正文 [mobile iPad]"), "[mobile iPad]");
        assert_eq!(corroboration_needle("裸文本无签名"), "裸文本无签名");
        assert_eq!(corroboration_needle(""), "");
    }

    // ===== 名额与锁 =====

    /// H4 全局名额：无名额 ⇒ **即时排队回执**（含全局位次），不盲试、不发 HTTP
    #[test]
    fn over_cap_turn_returns_queued_receipt_without_http() {
        let http = MockHttp::new();
        let fx = deps(http.clone(), Some(live_heartbeat()), Vec::new(), Vec::new());
        let hold_a = fx.sem.acquire();
        let hold_b = fx.sem.acquire();
        let hold_c = fx.sem.acquire();
        let hold_d = fx.sem.acquire();
        let out = run_turn(&args(), &fx);
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Queued,
            "{:?}",
            out.receipt
        );
        assert_eq!(
            out.receipt.queue_position(),
            Some(5),
            "4 个在飞 → 本请求第 5 位"
        );
        assert_eq!(http.calls().len(), 0, "排队请求不得起跑");
        drop((hold_a, hold_b, hold_c, hold_d));
        assert_eq!(fx.sem.in_flight(), 0, "名额必须归还（守卫 Drop）");
        http.assert_clean();
    }

    // ===== 「除了这条缝没有别的路」 =====

    /// 未预期请求必然违规 + 传输失败（**没有真实客户端兜底**）；默认钩子缝同样零网络
    #[test]
    fn mock_http_is_the_only_io_path() {
        let mut http = MockHttp::new();
        http.expect_post(CONNECT_PATH, None)
            .respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        // 只装一条期望：后续 initialize 请求未预期 ⇒ 违规且失败
        let fx = deps(http.clone(), Some(live_heartbeat()), Vec::new(), Vec::new());
        let out = run_turn(&args(), &fx);
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert!(
            out.receipt
                .reason
                .clone()
                .unwrap_or_default()
                .contains("未预期"),
            "未预期请求必须显式失败（不许静默继续）：{:?}",
            out.receipt.reason
        );
        assert!(!http.violations().is_empty(), "违规必须留痕");
        let seen = http.calls().len();
        // 默认钩子（未装填）：端点不可用 ⇒ 零 HTTP，绝不落真网络。
        // **并行纪律**（钩子是进程级全局态）：本段读写钩子，必须持 [`test_hooks::LOCK`]——
        // 否则会与端点用例互相 clear 对方装填的钩子/读到对方的假心跳（间歇性假红；
        // Task 11 复审 Important 1）。本用例是同步 `#[test]`，故用 `blocking_lock`
        // （与 `server.rs` 的 `#[tokio::test]` + `.lock().await` 同一把锁）。
        let _serial = test_hooks::LOCK.blocking_lock();
        let _clear = HookClear;
        test_hooks::clear();
        let fx2 = deps_for("windows", None, 600_000, GlobalSem::new(4));
        let out2 = run_turn(&args(), &fx2);
        assert_eq!(out2.receipt.stage, Some(Stage::Refused));
        assert_eq!(
            http.calls().len(),
            seen,
            "默认钩子下不得再有任何 HTTP 请求（真网络不可达）"
        );
    }

    /// 载荷组装：prompt 的 `prompt` 数组逐字（ACP 文本块）+ cwd 逐字进 load/new 参数
    #[test]
    fn rpc_wire_bodies_are_pinned() {
        let mut http = MockHttp::new();
        http.expect_post(CONNECT_PATH, None)
            .respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&init_sse(1));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&load_sse(2, None, "fullAccess"));
        http.expect_post(ACP_PATH, Some(ACP_HDRS))
            .respond_sse(&prompt_sse(
                3,
                &["model_requesting"],
                Some("end_turn"),
                None,
            ));
        let fx = deps(http.clone(), Some(live_heartbeat()), Vec::new(), Vec::new());
        let _ = run_turn(&args(), &fx);
        let calls = http.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].method, "POST");
        assert_eq!(calls[0].url, format!("{BASE}{CONNECT_PATH}"));
        assert!(calls[0].body.is_none(), "connect 无体（免鉴权）");
        let bodies: Vec<serde_json::Value> = calls[1..]
            .iter()
            .map(|c| serde_json::from_str(c.body.as_deref().unwrap()).unwrap())
            .collect();
        assert_eq!(bodies[0]["method"], "initialize");
        assert_eq!(bodies[0]["params"]["protocolVersion"], 1);
        assert_eq!(bodies[1]["method"], "session/load");
        assert_eq!(bodies[1]["params"]["sessionId"], SID);
        assert_eq!(bodies[1]["params"]["cwd"], CWD);
        assert_eq!(bodies[2]["method"], "session/prompt");
        assert_eq!(bodies[2]["params"]["sessionId"], SID);
        assert_eq!(
            bodies[2]["params"]["prompt"],
            serde_json::json!([{ "type": "text", "text": "probe [mobile iPad]" }])
        );
        // 逐请求 id 递增（响应帧按 id 认领）
        assert_eq!(bodies[0]["id"], 1);
        assert_eq!(bodies[1]["id"], 2);
        assert_eq!(bodies[2]["id"], 3);
        // 头三件套逐条对齐（含 Accept 逐字）
        for c in &calls[1..] {
            for (k, v) in [
                ("acp-connection-id", "c1"),
                ("acp-session-token", "t1"),
                ("accept", ACCEPT_BOTH),
            ] {
                assert_eq!(
                    c.headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(k))
                        .map(|(_, val)| val.as_str()),
                    Some(v),
                    "头 {k} 必须逐字（缺 Accept 服务端 -32000）"
                );
            }
        }
        http.assert_clean();
    }
}

//! 一次统计设置的所有权机制（Spec §5「一次配置与运行」；计划 Task 5）。
//!
//! ## 职责与边界
//!
//! 用户一次点击启用后，Tuvis 把**自有统计键**写进 Claude Code 的设置文件（`env` 对象），
//! 让 Harness 的原生 OTLP traces 指向本机接收器。铁律（Spec §5 逐条）：
//!
//! * **API/认证/provider/网关键绝不触碰**——写入键集固定为 [`STATISTICS_ENV_KEYS`]，
//!   全部是遥测/统计类键（证据：`docs/superpowers/evidence/2026-10-24-model-speed/`
//!   `otel-boundaries-addendum.md` §2 官方遥测键 + supplement 探针
//!   `raw/claude/CHANGE-PREVIEW.md` §1.2 在真机 2.1.293 上实测贯通的八键，
//!   `OTEL_EXPORTER_OTLP_HEADERS` 承载 Spec §5 要求的独立随机 Bearer 凭证）；
//! * **已有相同信号目标 → 显示冲突，默认保留不覆盖**；转交必须由
//!   `choice = TransferStatisticsTarget`（IPC 串 `transfer-statistics-target`）显式表达；
//! * **apply 时比较当前值与预览指纹**：预览后再变更（含过期）→ 拒绝，不落盘；
//! * **安装记录逐键保存** oldExists / oldValue / installedValue / 原指纹
//!   （[`InstallReceipt`]），只落 `~/.tuvis/tuvis.db` 的 tps_state（键前缀
//!   `install:`，见 store.rs 注册表）——用户私有存储，不进日志/报告；
//!   [`InstallReceipt`] 的 `Debug` 手工实现为**脱敏形态**（凭证/原值不打进日志面）；
//! * **卸载仅处理仍匹配安装值的自有项**：新增键删除、转交替换键恢复旧统计值、
//!   用户后改项保留并报冲突（部分清理后仍报 `Err`，冲突未清空不删安装记录）；
//!   **禁止整文件备份覆盖恢复**（永不写 backup 文件、永不整文件回灌——只做逐键
//!   JSON 读-改-写，未知键/顶层其它字段原样保留，serde_json preserve_order 保键序）；
//! * 敏感统计 header（[`SENSITIVE_ENV_KEYS`]）在 preview 只给**脱敏值/指纹**。
//!
//! ## 接收器守卫与持久化（Spec §5；socket accept loop 归 Task 7）
//!
//! * 只 bind 127.0.0.1；默认固定端口 [`DEFAULT_RECEIVER_PORT`]，首次被占则在
//!   [`FALLBACK_PORT_RANGE`] 顺延并持久保存（[`pick_receiver_port`] 纯函数 +
//!   `install:receiver:<tool>` 持久化）；之后的端口冲突报状态，不杀进程、不擅自换端口；
//! * OTLP 仅 JSON、路径 [`INGEST_PATH`]、上限 [`MAX_BODY_BYTES`]（1MiB）、
//!   独立随机 Bearer 凭证（[`verify_bearer`]）；
//! * Tuvis 退出即停，无系统守护进程（不落 launchd/计划任务，本模块也无此入口）。
//!
//! ## 测试隔离（2026-10-07 事故族纪律）
//!
//! 生产入口 [`preview`] / [`apply`] / [`uninstall`] 以**真实 home** 解析；测试必须用
//! `*_at(home, …)` 注入形态传 tempdir——任何用例不得触真机 `~/.claude` / `~/.tuvis`。
//!
//! ## 错误码（码表登记纪律：只用 `USAGE_CODES` 已登记码，不新增）
//!
//! * 语义拒绝（冲突 / 预览过期 / 预览后变更 / 解析失败 / 未知工具）→
//!   `usage-settings-invalid`（detail 前缀 `tps-install-*` / `tps-uninstall-*` 定位；
//!   新码登记要同步前端三处，归 Task 7/8 的 IPC 面一并处理）；
//! * 设置文件 IO / 状态库 → `usage-source-io` / `usage-db-failed`；序列化 → `usage-internal`。

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::database::schema;
use crate::services::tps::store;
use crate::services::usage::error::UsageError;

// ==== 支持的工具与设置文件定位 ====

/// 本任务支持一次统计配置的工具（Claude Code；OpenCode 插件复用本机制归 Task 6）
pub const SUPPORTED_TOOLS: &[&str] = &["claude", "opencode"];

/// 工具分派（安装目标完全不同：claude = 设置文件 env 键；opencode = 自有插件文件）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    Claude,
    OpenCode,
}

fn tool_kind(tool: &str) -> Result<ToolKind, UsageError> {
    match tool {
        "claude" => Ok(ToolKind::Claude),
        "opencode" => Ok(ToolKind::OpenCode),
        other => Err(UsageError::new(
            "usage-settings-invalid",
            format!(
                "tps-install-unknown-tool: unknown tool {other} (supported: {SUPPORTED_TOOLS:?})"
            ),
        )),
    }
}

/// Claude 设置文件：`<home>/.claude/settings.json`。
/// 与 `adapter/claude.rs::ClaudeAdapter::base_dir()` + `hook_config_path()` 的
/// `settings.json` 同源（镜像实现以便 home 可注入；单测锁两处一致）。
fn settings_path_for(home: &Path, tool: &str) -> Result<PathBuf, UsageError> {
    if tool_kind(tool)? != ToolKind::Claude {
        return Err(UsageError::new(
            "usage-settings-invalid",
            format!(
                "tps-install-tool-mismatch: install target of tool {tool} is not a settings file"
            ),
        ));
    }
    Ok(home.join(".claude").join("settings.json"))
}

// ==== OpenCode 观察插件分发（Task 6；嵌入资源 + 自有文件所有权）====

/// 嵌入的插件分发（include_str! 让资源**进二进制**，不只开发仓库可用——brief Step3）。
/// 路径相对本文件：`<repo>/resources/tps/opencode/`。
pub const OPENCODE_PLUGIN_JS: &str =
    include_str!("../../../../resources/tps/opencode/tuvis-tps-observer.ts");
pub const OPENCODE_PLUGIN_MANIFEST: &str =
    include_str!("../../../../resources/tps/opencode/package.json");
pub const OPENCODE_PLUGIN_LICENSE: &str =
    include_str!("../../../../resources/tps/opencode/LICENSE");
pub const OPENCODE_PLUGIN_NOTICE: &str =
    include_str!("../../../../resources/tps/opencode/NOTICE.md");

/// 自有插件文件名（与用户既有插件文件名空间隔离；同名冲突 = 拒绝，不覆盖）
pub const OPENCODE_PLUGIN_FILE: &str = "tuvis-tps-observer.ts";

/// OpenCode 插件目录：`<home>/.config/opencode/plugins/`。
/// 宿主 2.0.22 目录发现含 `"plugin"` 与 `"plugins"` 两名（二进制
/// `var d5=["plugin","plugins"]`）；用户真机在用 `plugins/`（探针插件同目录已装载），
/// 故取 `plugins/`。**只写自有文件，不改 opencode.json 等用户配置**（目录发现自动装载，
/// 无需配置项；也绝不触碰用户其他插件/依赖）。
fn opencode_plugin_path_for(home: &Path) -> PathBuf {
    home.join(".config")
        .join("opencode")
        .join("plugins")
        .join(OPENCODE_PLUGIN_FILE)
}

/// 读取目标文件现内容（缺失 = None；读失败 = Err，不猜）
fn read_own_file(path: &Path) -> Result<Option<String>, UsageError> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read_to_string(path).map(Some).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!("tps: failed to read plugin file ({}): {e}", path.display()),
        )
    })
}

// ==== 目标统计键集（固定；证据见模块头）====

/// 自有统计键（顺序即 preview 展示与安装记录顺序）。
///
/// 官方文档要求 `CLAUDE_CODE_ENABLE_TELEMETRY` 与 `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA`
/// 加 `OTEL_TRACES_EXPORTER` 三键开 traces；`OTEL_METRICS_EXPORTER`/`OTEL_LOGS_EXPORTER`
/// 置 none 降噪（metrics 面无时延、logs 会带正文——探针 §0/§1.2 实测决策）；
/// `OTEL_EXPORTER_OTLP_PROTOCOL=http/json`（接收器只解 JSON）；endpoint 指向本机固定
/// 端口；headers 承载独立 Bearer 凭证；导出间隔 1s 保证可见期 5s 读取的新鲜度。
pub const STATISTICS_ENV_KEYS: [&str; 9] = [
    "CLAUDE_CODE_ENABLE_TELEMETRY",
    "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA",
    "OTEL_TRACES_EXPORTER",
    "OTEL_METRICS_EXPORTER",
    "OTEL_LOGS_EXPORTER",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_TRACES_EXPORT_INTERVAL",
];

/// 敏感键：凭证承载 header——preview 只给脱敏值/指纹，记录 Debug 脱敏
pub const SENSITIVE_ENV_KEYS: &[&str] = &["OTEL_EXPORTER_OTLP_HEADERS"];

fn is_sensitive(key: &str) -> bool {
    SENSITIVE_ENV_KEYS.contains(&key)
}

/// 某统计键的安装值（port/credential 由接收器配置注入）
fn planned_value_for(key: &str, receiver: &ReceiverConfig) -> String {
    match key {
        "CLAUDE_CODE_ENABLE_TELEMETRY" => "1".into(),
        "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA" => "1".into(),
        "OTEL_TRACES_EXPORTER" => "otlp".into(),
        "OTEL_METRICS_EXPORTER" => "none".into(),
        "OTEL_LOGS_EXPORTER" => "none".into(),
        "OTEL_EXPORTER_OTLP_PROTOCOL" => "http/json".into(),
        "OTEL_EXPORTER_OTLP_ENDPOINT" => format!("http://127.0.0.1:{}", receiver.port),
        "OTEL_EXPORTER_OTLP_HEADERS" => {
            format!("Authorization=Bearer {}", receiver.credential)
        }
        "OTEL_TRACES_EXPORT_INTERVAL" => "1000".into(),
        other => {
            unreachable!("key {other} outside STATISTICS_ENV_KEYS should not have a planned value")
        }
    }
}

// ==== 接收器守卫常量与纯函数（Spec §5）====

/// 默认固定端口（Spec §5「默认固定端口47181」）
pub const DEFAULT_RECEIVER_PORT: u16 = 47_181;
/// 首次被占时的顺延区间（含端点；之后冲突报状态，不再换端口）
pub const FALLBACK_PORT_RANGE: std::ops::RangeInclusive<u16> = 47_182..=47_190;
/// 请求体上限：1MiB（Spec §5「1MiB上限」）
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// 唯一接收路径（OTLP 仅 JSON traces）
pub const INGEST_PATH: &str = "/v1/traces";

/// 端口选择（纯函数；`occupied` = 本机已占用端口探测结果，由 Task 7 绑定前喂入）：
/// 默认 47181；被占则在 47182–47190 顺延取第一个空闲；全占用 → None（调用方报状态，
/// **绝不**杀进程或静默改端口使已装配置失效）。
pub fn pick_receiver_port(occupied: &[u16]) -> Option<u16> {
    if !occupied.contains(&DEFAULT_RECEIVER_PORT) {
        return Some(DEFAULT_RECEIVER_PORT);
    }
    FALLBACK_PORT_RANGE.clone().find(|p| !occupied.contains(p))
}

/// 生成独立随机 Bearer 凭证（64 hex；与 API 认证完全无关，只用于本机接收器自证身份）
pub fn generate_receiver_credential() -> String {
    format!(
        "{:032x}{:032x}",
        rand::random::<u128>(),
        rand::random::<u128>()
    )
}

/// 请求体上限守卫（超限拒绝，不读不解析）
pub fn enforce_body_limit(len: usize) -> Result<(), UsageError> {
    if len <= MAX_BODY_BYTES {
        Ok(())
    } else {
        Err(UsageError::new(
            "usage-source-io",
            format!(
                "tps-ingest-oversize: body {len} bytes exceeds 1MiB ({MAX_BODY_BYTES}) limit, rejecting"
            ),
        ))
    }
}

/// 路径守卫：只接受 `/v1/traces`（不开 metrics/logs 面）
pub fn verify_ingest_path(path: &str) -> Result<(), UsageError> {
    if path == INGEST_PATH {
        Ok(())
    } else {
        Err(UsageError::new(
            "usage-source-io",
            format!(
                "tps-ingest-path: only {INGEST_PATH} accepted (OTLP JSON traces only), got {path}"
            ),
        ))
    }
}

/// 常量时间字节比较（长度相等前提下的逐字节异或折叠；凭证长度固定 64，长度差异不泄漏）
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Bearer 凭证守卫：缺头 / 非 Bearer 方案 / 凭证不符 → 拒绝（Spec §5「独立随机Bearer凭证」）
pub fn verify_bearer(header: Option<&str>, expected: &str) -> Result<(), UsageError> {
    let Some(header) = header else {
        return Err(UsageError::new(
            "usage-source-io",
            "tps-ingest-unauthorized: missing Authorization header, rejecting",
        ));
    };
    let Some(credential) = header.strip_prefix("Bearer ") else {
        return Err(UsageError::new(
            "usage-source-io",
            "tps-ingest-unauthorized: non-Bearer scheme, rejecting",
        ));
    };
    if ct_eq(credential.trim().as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(UsageError::new(
            "usage-source-io",
            "tps-ingest-unauthorized: Bearer credential mismatch, rejecting",
        ))
    }
}

// ==== 接收器配置持久化（tps_state：install:receiver:<tool>）====

/// 接收器运行身份（端口 + 独立凭证；安装时生成、重启后复用——凭证轮换会让已装
/// settings 的 header 失效，故只在显式重装流程里更换）
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiverConfig {
    pub port: u16,
    pub credential: String,
    pub created_at_ms: i64,
}

impl std::fmt::Debug for ReceiverConfig {
    /// 脱敏 Debug（凭证不进日志面；「记录不进日志/报告」纪律的日志半边）
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiverConfig")
            .field("port", &self.port)
            .field(
                "credential",
                &format!("[redacted {}]", fingerprint(&self.credential)),
            )
            .field("created_at_ms", &self.created_at_ms)
            .finish()
    }
}

/// 接收器配置键（Spec §5「首次占用则在47182—47190中选择并持久保存」）
pub fn receiver_state_key(tool: &str) -> String {
    format!("install:receiver:{tool}")
}

/// 读回持久化的接收器配置（未安装过 = None）
pub fn load_receiver_config_at(
    home: &Path,
    tool: &str,
) -> Result<Option<ReceiverConfig>, UsageError> {
    let conn = open_state_db(home)?;
    match store::load_state(&conn, &receiver_state_key(tool))? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| UsageError::internal(format!("tps: receiver config parse failed: {e}"))),
    }
}

fn save_receiver_config(
    conn: &Connection,
    tool: &str,
    receiver: &ReceiverConfig,
) -> Result<(), UsageError> {
    let value = serde_json::to_value(receiver).map_err(|e| {
        UsageError::internal(format!("tps: receiver config serialization failed: {e}"))
    })?;
    store::save_state(conn, &receiver_state_key(tool), &value)
}

// ==== 安装选择 / 预览 / 回执（IPC 面类型，serde 走 Spec §6 camelCase 镜像）====

/// 安装选择（Spec §6 `choice`；IPC 串 kebab-case）：
/// * `AppendOnly`（默认）：只新增，已有相同信号目标 → 拒绝（默认保留不覆盖）；
/// * `TransferStatisticsTarget`：用户显式转交——仅替换已授权的冲突统计键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallChoice {
    AppendOnly,
    TransferStatisticsTarget,
}

/// 预览条目（IPC 面：只含**脱敏值/指纹**，不含任何全文凭证）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPreviewEntry {
    pub key: String,
    /// 预览时该键是否已存在
    pub old_exists: bool,
    /// 预览时旧值（敏感键脱敏；无旧值 = null）
    pub old_masked: Option<String>,
    /// 预览时旧值指纹（sha256 前 16 hex；无旧值 = null）
    pub old_fingerprint: Option<String>,
    /// 计划安装值（敏感键脱敏）
    pub planned_masked: String,
    /// 已有相同信号目标（旧值存在且 ≠ 计划值）
    pub conflict: bool,
}

/// 安装预览（Spec §6「安装preview含目标项/原值指纹/自有项」）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPreview {
    pub preview_id: String,
    pub tool: String,
    /// 目标设置文件（绝对路径串，供 UI 展示）
    pub settings_path: String,
    /// 过期时刻（epoch ms；过期后 apply 拒绝）
    pub expires_at_ms: i64,
    pub entries: Vec<InstallPreviewEntry>,
    /// 冲突键清单（非空时 AppendOnly 拒绝应用）
    pub conflict_keys: Vec<String>,
}

/// 单键安装记录（Spec §5「逐键保存原先是否存在、原统计值、安装值及指纹」；
/// `old_value` 保存**原样 JSON 值**（含非字符串旧值的精确类型，卸载时原样恢复）——
/// 仅落 tps_state 用户私有存储，Debug/日志面脱敏）
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReceiptEntry {
    pub key: String,
    pub old_exists: bool,
    pub old_value: Option<serde_json::Value>,
    pub installed_value: String,
    pub old_fingerprint: Option<String>,
}

impl std::fmt::Debug for InstallReceiptEntry {
    /// 脱敏 Debug：值面一律指纹化（记录「不进日志/报告」纪律的日志半边）
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallReceiptEntry")
            .field("key", &self.key)
            .field("old_exists", &self.old_exists)
            .field(
                "old_value",
                &self
                    .old_value
                    .as_ref()
                    .map(|v| format!("[{}]", fingerprint(&value_canonical(v)))),
            )
            .field(
                "installed_value",
                &format!("[{}]", fingerprint(&self.installed_value)),
            )
            .field("old_fingerprint", &self.old_fingerprint)
            .finish()
    }
}

/// 安装回执（apply 返回值 + tps_state 安装记录 `install:record:<tool>`）。
/// `receiver` 只属 claude（OTLP 接收器身份）；opencode 无接收器（插件本地落盘）= None。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReceipt {
    pub tool: String,
    pub applied_at_ms: i64,
    pub choice: InstallChoice,
    pub entries: Vec<InstallReceiptEntry>,
    #[serde(default)]
    pub receiver: Option<ReceiverConfig>,
}

impl std::fmt::Debug for InstallReceipt {
    /// 脱敏 Debug（凭证不进日志面）
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallReceipt")
            .field("tool", &self.tool)
            .field("applied_at_ms", &self.applied_at_ms)
            .field("choice", &self.choice)
            .field("entries", &self.entries)
            .field("receiver", &self.receiver)
            .finish()
    }
}

/// 安装记录键（tps_state；键前缀注册表见 store.rs）
pub fn record_state_key(tool: &str) -> String {
    format!("install:record:{tool}")
}

/// 读取既有安装回执（显式 null = 已卸载/已了结 → 视作不存在）。
/// apply 路径用它沿用**首次接管**的所有权事实：重装/升级时当前值若等于我们自己的
/// 安装值，oldExists/oldValue 必须保持首次记录，不得被自有安装值重置（独立复核 R2）。
fn load_receipt(conn: &Connection, tool: &str) -> Result<Option<InstallReceipt>, UsageError> {
    let value = store::load_state(conn, &record_state_key(tool))?.filter(|v| !v.is_null());
    value
        .map(|v| {
            serde_json::from_value::<InstallReceipt>(v)
                .map_err(|e| UsageError::internal(format!("tps: install record parse failed: {e}")))
        })
        .transpose()
}

/// 预览记录键（apply 凭 id 取回完整计划值与指纹快照）
pub fn preview_state_key(preview_id: &str) -> String {
    format!("install:preview:{preview_id}")
}

/// 每工具**唯一**的待应用预览槽（`install:pending:<tool>` → preview_id）。
/// 新预览先即焚槽内旧预览记录——待处理凭证至多 1 份/工具（DAO 无删除通道，否则
/// 重复预览会让废弃预览连同其内嵌 bearer 凭证永久持久化——评审 Important 3b）。
pub fn pending_state_key(tool: &str) -> String {
    format!("install:pending:{tool}")
}

/// 预览有效期（10 分钟：一次点击确认的合理窗口；过期必须重新 preview）
const PREVIEW_TTL_MS: i64 = 10 * 60 * 1000;

/// 值指纹：sha256 前 16 hex（`sha256:<hex>`；不泄漏原值）
fn fingerprint(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    let head: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("sha256:{head}")
}

/// 受管键当前值的**规范化串**（指纹与「是否有目标」判定的唯一基准）：
/// 字符串取原串；非字符串旧值（数字/布尔/对象…）取 JSON 序列化串——键存在但非
/// 字符串**绝不**等同不存在（评审 Important 1：as_str() 静默吞掉会让预览漏冲突、
/// apply 覆盖用户值，违反 §5「默认保留，不覆盖」）。
fn value_canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn mask_value(key: &str, value: &str) -> String {
    if is_sensitive(key) {
        format!("[redacted {}]", fingerprint(value))
    } else {
        value.to_string()
    }
}

// ==== tps_state 连接（安装记录属低频一次性操作，自开连接，不挂全局 DB）====

fn open_state_db(home: &Path) -> Result<Connection, UsageError> {
    let dir = home.join(".tuvis");
    std::fs::create_dir_all(&dir).map_err(|e| {
        UsageError::new(
            "usage-db-failed",
            format!("tps: failed to create data dir: {e}"),
        )
    })?;
    let db_path = dir.join("tuvis.db");
    let conn = Connection::open(&db_path).map_err(|e| {
        UsageError::new(
            "usage-db-failed",
            format!("tps: failed to open state db: {e}"),
        )
    })?;
    // busy_timeout 1s：tuvis.db 默认 journal 模式下，采集轮持写事务的瞬间，接收器每
    // POST 的配置读 / 安装流撞写锁会立即 SQLITE_BUSY（接收器 503、安装报错需重试）。
    // 低频路径等 1s 远优于直接失败；本函数同时服务 install 与 receiver 两条路径。
    conn.busy_timeout(std::time::Duration::from_millis(1000))
        .map_err(|e| {
            UsageError::new(
                "usage-db-failed",
                format!("tps: failed to set busy timeout: {e}"),
            )
        })?;
    // 安装记录逐键保存原值/凭证，Spec §5 要求「用户私有，Unix 0600」——既有 database
    // 层按 umask 默认建库、无权限加固（src-tauri/src/database/connection.rs:54-56/:108，
    // 全 database/ 无 set_permissions）→ 本路径收口补齐（幂等：已 0600 时为无害重设）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600)).map_err(
            |e| {
                UsageError::new(
                    "usage-db-failed",
                    format!("tps: failed to tighten state db permissions: {e}"),
                )
            },
        )?;
    }
    // 幂等建表：生产库已初始化时无副作用；测试私有库靠它建齐 tps_state
    schema::init(&conn);
    Ok(conn)
}

// ==== 设置文件读-改-写（只动 env 子树；未知键/顶层字段/键序全保留）====

/// 读取设置文件：缺失 = 空对象（全新增态）；解析失败 → Err（**不落盘**纪律）。
/// 返回 (根对象, 文件是否已存在)——新建文件走 0600 加固。
fn read_settings(path: &Path) -> Result<(serde_json::Value, bool), UsageError> {
    if !path.exists() {
        return Ok((serde_json::json!({}), false));
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!("tps: failed to read settings ({}): {e}", path.display()),
        )
    })?;
    let root: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        UsageError::new(
            "usage-settings-invalid",
            format!(
                "tps-install-settings-parse: failed to parse {} (refusing to write, nothing persisted): {e}",
                path.display()
            ),
        )
    })?;
    Ok((root, true))
}

/// 取（或建）顶层 `env` 对象；被非对象占用 → 拒绝（不静默覆盖用户数据）
fn env_obj_mut(
    root: &mut serde_json::Value,
) -> Result<&mut serde_json::Map<String, serde_json::Value>, UsageError> {
    let obj = root.as_object_mut().ok_or_else(|| {
        UsageError::new(
            "usage-settings-invalid",
            "tps-install-settings-shape: settings root is not a JSON object, refusing to write",
        )
    })?;
    if !obj.contains_key("env") {
        obj.insert("env".into(), serde_json::json!({}));
    }
    // 重新借（避免 match 持有第一次可变借用跨过 insert）
    match obj.get_mut("env") {
        Some(v) if v.is_object() => Ok(v.as_object_mut().expect("is_object checked above")),
        Some(_) => Err(UsageError::new(
            "usage-settings-invalid",
            "tps-install-env-shape: settings env section is occupied by a non-object value, refusing to write (user data not overwritten)",
        )),
        None => unreachable!("上一行刚保证 env 键存在"),
    }
}

/// 写回设置文件（整文件重写但**内容 = 原文 + 受管键差量**：未知键与键序由
/// serde_json preserve_order 原样保留）。Unix：临时文件以目标 mode（已存在 = 原
/// 权限位；新建 = 0600）创建后 rename——mode 随 rename 落盘，无需写后二次 chmod。
fn write_settings(path: &Path, root: &serde_json::Value, existed: bool) -> Result<(), UsageError> {
    // 先取原权限位（temp+rename 落盘会重置文件模式，临时文件按它创建）
    #[cfg(unix)]
    let prior_mode: Option<u32> = std::fs::metadata(path).ok().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o777
    });
    let _ = existed; // unix 分支由 prior_mode 隐含（None = 新建）；非 unix 分支见下
    let mut pretty = serde_json::to_string_pretty(root)
        .map_err(|e| UsageError::internal(format!("tps: settings serialization failed: {e}")))?;
    pretty.push('\n');
    // 原子落盘走**本模块私有写**：临时文件以目标 mode（0600/原 mode）创建——设置文件
    // 含用户认证键与新 Bearer 凭证，绝不经共享 helper 的默认 umask（常 0644）落盘
    // （评审 Important 2；linker::write_config_locked 的行为其他调用方依赖，不改）。
    // 临时文件 mode 即最终 mode（rename 保留），无需写后二次 chmod。
    #[cfg(unix)]
    {
        let target_mode = prior_mode.unwrap_or(0o600); // 新建 = 用户私有 0600（Spec §5）
        write_config_atomic_private(path, &pretty, target_mode)?;
    }
    #[cfg(not(unix))]
    {
        // TODO(platform): Windows 用户私有 ACL（仅当前用户可读）尚未显式设置——
        // Task 9 两平台收口前补齐；当前新建文件继承父目录 ACL（%USERPROFILE% 下
        // 默认已用户私有），已存在文件保持原 ACL 不动；私有原子写与 Unix 同构
        // （锁 + 临时文件 + rename），不经共享 helper 的默认 umask 落盘。
        // mode 实参在 Windows 实现中被忽略（无 POSIX 权限位），传 0600 仅保持签名一致。
        write_config_atomic_private(path, &pretty, 0o600)?;
    }
    Ok(())
}

/// install 私有的原子写（Unix）：目标文件加排它锁（与共享 helper 同款防并发
/// 读-改-写互踩），临时文件以**指定 mode** 创建后 rename 覆盖——写与 rename 之间
/// 进程死亡也只残留 0600 的 `settings.tmp`，绝不世界可读。
#[cfg(unix)]
fn write_config_atomic_private(path: &Path, content: &str, mode: u32) -> Result<(), UsageError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // 与 linker::write_config_locked 同款排它锁（fs2::FileExt）
    use fs2::FileExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!(
                    "tps: failed to create settings dir ({}): {e}",
                    path.display()
                ),
            )
        })?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        // 崩溃残留的空锁文件若走默认 umask（0644）落盘，会让后续 parse 拒绝世界可读
        // 的 settings；mode 随调用方的 prior_mode（新建 = 0600，Spec §5）落盘。
        .mode(mode)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps: failed to open settings ({}): {e}", path.display()),
            )
        })?;
    lock.lock_exclusive().map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!(
                "tps: failed to lock settings file ({}): {e}",
                path.display()
            ),
        )
    })?;
    let result = (|| {
        let temp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(mode)
            .open(&temp)
            .map_err(|e| {
                UsageError::new(
                    "usage-source-io",
                    format!(
                        "tps: failed to create settings temp file ({}, mode {mode:o}): {e}",
                        temp.display()
                    ),
                )
            })?;
        f.write_all(content.as_bytes()).map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps: failed to write settings content: {e}"),
            )
        })?;
        std::fs::rename(&temp, path).map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps: failed to persist settings ({}): {e}", path.display()),
            )
        })?;
        Ok(())
    })();
    let _ = lock.unlock();
    result
}

/// install 私有的原子写（Windows）：与 Unix 版同构——排它锁防并发读-改-写互踩，
/// 临时文件写入后 rename 原子替换（std::fs::rename 在 Windows 上替换已存在目标）。
/// `mode` 参数仅为两侧调用点签名一致而保留：Unix 语义是 0600/原 mode 权限位落盘，
/// Windows 无对应权限位模型——文件私有性继承父目录 ACL（%USERPROFILE% 下默认仅
/// 当前用户可读），显式用户私有 ACL 仍为 Task 9 两平台收口的 cfg+TODO（与设置
/// 写路径同款申报）；临时文件同样继承该 ACL，绝不经世界可读默认路径。
#[cfg(not(unix))]
fn write_config_atomic_private(path: &Path, content: &str, mode: u32) -> Result<(), UsageError> {
    use std::io::Write;
    // 与 linker::write_config_locked 同款排它锁（fs2::FileExt，Windows 亦支持）
    use fs2::FileExt;
    let _ = mode; // Windows 无 POSIX 权限位；私有性由父目录 ACL 承载（见函数头）
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!(
                    "tps: failed to create settings dir ({}): {e}",
                    path.display()
                ),
            )
        })?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps: failed to open settings ({}): {e}", path.display()),
            )
        })?;
    lock.lock_exclusive().map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!(
                "tps: failed to lock settings file ({}): {e}",
                path.display()
            ),
        )
    })?;
    let result = (|| {
        let temp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)
            .map_err(|e| {
                UsageError::new(
                    "usage-source-io",
                    format!(
                        "tps: failed to create settings temp file ({}): {e}",
                        temp.display()
                    ),
                )
            })?;
        f.write_all(content.as_bytes()).map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps: failed to write settings content: {e}"),
            )
        })?;
        std::fs::rename(&temp, path).map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps: failed to persist settings ({}): {e}", path.display()),
            )
        })?;
        Ok(())
    })();
    let _ = lock.unlock();
    result
}

// ==== 预览 / 应用 / 卸载 ====

/// 预览记录（tps_state 内部形态：全文计划值 + 逐键指纹快照；apply 凭此比对漂移）
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedPreview {
    preview_id: String,
    tool: String,
    created_at_ms: i64,
    expires_at_ms: i64,
    /// 接收器身份（仅 claude；opencode 插件本地落盘无接收器 = None）
    #[serde(default)]
    receiver: Option<ReceiverConfig>,
    entries: Vec<PersistedEntry>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedEntry {
    key: String,
    planned_value: String,
    /// 预览时该键的值指纹（无值 = None）——apply 时的漂移比对基准
    then_fingerprint: Option<String>,
    /// 预览时旧值是否为**非字符串**（评审 Round 2：形状也是判定面——规范化串碰巧
    /// 等于计划串的数字/布尔旧值如 `1`/`1000` 仍须冲突/替换，不能只比规范化串）
    #[serde(default)]
    then_non_string: bool,
    /// 预览时旧目标**全文**（仅 opencode 文件目标用：transfer 替换后卸载需原样恢复；
    /// claude env 键的旧值小，apply 现场即可取回，保持 None）
    #[serde(default)]
    then_value: Option<String>,
}

/// 生成安装预览（纯读设置文件 + 写 tps_state 预览记录；**绝不改设置文件**）。
/// 生产入口（真实 home；端口身份来自**真实 bind 探测**——接收器新装路径的端口
/// 必须在持久化前真的试绑过一次，Task 7 收口 Task 5 的「空占用表选端口」缺口）。
pub fn preview(tool: &str) -> Result<InstallPreview, UsageError> {
    preview_with_port_probe(&runtime_home(), tool, &|| {
        super::receiver::probe_free_port()
    })
}

/// 同 [`preview`]，home 注入形态（测试隔离唯一入口）。
/// 端口身份走**纯**选择（`pick_receiver_port` 空占用表——不 bind 任何 socket，守住
/// 测试「绝不 bind 47181–47190」纪律）；需要注入探测的用例走 [`preview_with_port_probe`]。
pub fn preview_at(home: &Path, tool: &str) -> Result<InstallPreview, UsageError> {
    preview_with_port_probe(home, tool, &|| {
        Some(pick_receiver_port(&[]).expect("空占用表必得默认端口"))
    })
}

/// [`preview`] 的探测注入形态：`probe` 返回本次新装使用的接收器端口（None = 全占用，
/// 报 `tps-receiver-port-exhausted` 状态拒绝预览——绝不持久化一个未试绑过的端口）。
/// 已有持久化配置时复用（端口/凭证轮换会让已装 header 失效），探测不调用。
pub fn preview_with_port_probe(
    home: &Path,
    tool: &str,
    probe: &dyn Fn() -> Option<u16>,
) -> Result<InstallPreview, UsageError> {
    if tool_kind(tool)? == ToolKind::OpenCode {
        return preview_opencode_at(home);
    }
    let settings_path = settings_path_for(home, tool)?;
    let conn = open_state_db(home)?;
    // 接收器身份：已装 → 复用；未装 → 经探测现配（生产 = 真实 bind，测试 = 注入/纯）
    let receiver = match load_receiver_config_at(home, tool)? {
        Some(r) => r,
        None => {
            let port = probe().ok_or_else(|| {
                UsageError::new(
                    "usage-source-io",
                    format!(
                        "tps-receiver-port-exhausted: receiver candidate ports {}-{} are all \\
                         occupied; report status per Spec §5, never kill processes or switch \\
                         ports silently",
                        DEFAULT_RECEIVER_PORT,
                        FALLBACK_PORT_RANGE.end()
                    ),
                )
            })?;
            ReceiverConfig {
                port,
                credential: generate_receiver_credential(),
                created_at_ms: crate::services::usage::now_ms(),
            }
        }
    };

    let (root, _existed) = read_settings(&settings_path)?;
    let env = root.get("env").and_then(|v| v.as_object());

    let now = crate::services::usage::now_ms();
    let mut entries = Vec::with_capacity(STATISTICS_ENV_KEYS.len());
    let mut persisted = Vec::with_capacity(STATISTICS_ENV_KEYS.len());
    let mut conflict_keys = Vec::new();
    for key in STATISTICS_ENV_KEYS {
        // 规范化读取：键存在但非字符串（数字/布尔/对象…）同样算「已有相同信号目标」
        //（评审 Important 1：绝不静默当作不存在——那会让预览漏冲突、apply 覆盖用户值）
        let old_canonical = env.and_then(|e| e.get(key)).map(value_canonical);
        let old_is_non_string = matches!(env.and_then(|e| e.get(key)), Some(v) if !v.is_string());
        let old_fingerprint = old_canonical.as_deref().map(fingerprint);
        let planned = planned_value_for(key, &receiver);
        // 冲突判定含**形状**：非字符串旧值恒为冲突——即使规范化串碰巧等于计划串
        //（如数字 1 vs "1"、数字 1000 vs "1000"）：JSON 数字/布尔 env 是否被
        // Claude Code 当字符串尊重未知，保留原形会让遥测静默失效（评审 Round 2）
        let conflict = old_is_non_string || matches!(&old_canonical, Some(o) if *o != planned);
        if conflict {
            conflict_keys.push((*key).to_string());
        }
        let old_masked = match (&old_canonical, old_is_non_string) {
            (Some(o), false) => Some(mask_value(key, o)),
            // 非字符串旧值无论键敏感与否一律只给指纹（内容形态未知，不外显）
            (Some(o), true) => Some(format!("[non-string value {}]", fingerprint(o))),
            (None, _) => None,
        };
        entries.push(InstallPreviewEntry {
            key: (*key).to_string(),
            old_exists: old_canonical.is_some(),
            old_masked,
            old_fingerprint: old_fingerprint.clone(),
            planned_masked: mask_value(key, &planned),
            conflict,
        });
        persisted.push(PersistedEntry {
            key: (*key).to_string(),
            planned_value: planned,
            then_fingerprint: old_fingerprint.clone(),
            then_non_string: old_is_non_string,
            then_value: None,
        });
    }

    let preview_id = format!("pv-{:032x}", rand::random::<u128>());
    let record = PersistedPreview {
        preview_id: preview_id.clone(),
        tool: tool.to_string(),
        created_at_ms: now,
        expires_at_ms: now + PREVIEW_TTL_MS,
        receiver: Some(receiver),
        entries: persisted,
    };
    let value = serde_json::to_value(&record)
        .map_err(|e| UsageError::internal(format!("tps: preview serialization failed: {e}")))?;
    // 即焚被替换的待应用预览（评审 Important 3b：待处理凭证至多 1 份/工具——
    // 重复预览不再堆积废弃预览记录与其内嵌 bearer 凭证）
    let pending_id = store::load_state(&conn, &pending_state_key(tool))?
        .and_then(|v| v.as_str().map(str::to_string));
    if let Some(old_id) = pending_id {
        if old_id != preview_id {
            store::save_state(&conn, &preview_state_key(&old_id), &serde_json::Value::Null)?;
        }
    }
    store::save_state(&conn, &preview_state_key(&preview_id), &value)?;
    store::save_state(
        &conn,
        &pending_state_key(tool),
        &serde_json::json!(preview_id),
    )?;

    Ok(InstallPreview {
        preview_id,
        tool: tool.to_string(),
        settings_path: settings_path.display().to_string(),
        expires_at_ms: record.expires_at_ms,
        entries,
        conflict_keys,
    })
}

/// 应用预览（Spec §6：apply 比较当前值与预览指纹，预览后再变更则拒绝；
/// transfer 且指纹未变时仅替换已授权统计键）。生产入口；测试用 [`apply_at`]。
pub fn apply(preview_id: &str, choice: InstallChoice) -> Result<InstallReceipt, UsageError> {
    apply_at(&runtime_home(), preview_id, choice)
}

/// 同 [`apply`]，home 注入形态（测试隔离唯一入口）
pub fn apply_at(
    home: &Path,
    preview_id: &str,
    choice: InstallChoice,
) -> Result<InstallReceipt, UsageError> {
    let conn = open_state_db(home)?;
    // 「即焚」标记是显式 null（dao 无删除通道）→ 读侧把 null 视作不存在
    let value = store::load_state(&conn, &preview_state_key(preview_id))?.filter(|v| !v.is_null());
    let Some(value) = value else {
        return Err(UsageError::new(
            "usage-settings-invalid",
            format!("tps-install-preview-not-found: preview {preview_id} not found (already applied / expired cleanup?), re-run preview"),
        ));
    };
    let record: PersistedPreview = serde_json::from_value(value)
        .map_err(|e| UsageError::internal(format!("tps: preview record parse failed: {e}")))?;
    if crate::services::usage::now_ms() > record.expires_at_ms {
        return Err(UsageError::new(
            "usage-settings-invalid",
            format!("tps-install-preview-expired: preview {preview_id} expired, re-run preview"),
        ));
    }
    // 工具分派：OpenCode 走自有插件文件路径（所有权语义同款：指纹漂移拒绝 / 冲突不覆盖）
    if tool_kind(&record.tool)? == ToolKind::OpenCode {
        return apply_opencode_at(home, &conn, preview_id, record, choice);
    }

    // 逐键指纹比对：预览后设置任何受管键漂移 → 拒绝（不落盘）。指纹基于规范化值
    //（字符串原串 / 非字符串 JSON 序列化串），键存在但非字符串不再等同不存在
    let settings_path = settings_path_for(home, &record.tool)?;
    let (mut root, existed) = read_settings(&settings_path)?;
    let env = env_obj_mut(&mut root)?;
    for entry in &record.entries {
        let current_fp = env
            .get(&entry.key)
            .map(value_canonical)
            .as_deref()
            .map(fingerprint);
        if current_fp != entry.then_fingerprint {
            return Err(UsageError::new(
                "usage-settings-invalid",
                format!(
                    "tps-install-preview-stale: key {} changed after preview (current fingerprint {:?} != preview fingerprint {:?}), refusing to apply; re-run preview",
                    entry.key, current_fp, entry.then_fingerprint
                ),
            ));
        }
    }
    // 冲突键 = 预览时已有值且（形状非字符串 或 ≠ 计划值）——形状不等恒为冲突，
    // 规范化串碰巧相等的数字/布尔旧值不能绕过 append-only 的保留语义（Round 2）
    let conflict_keys: Vec<String> = record
        .entries
        .iter()
        .filter(|e| {
            e.then_non_string
                || match e.then_fingerprint.as_deref() {
                    None => false,
                    Some(then) => then != fingerprint(&e.planned_value),
                }
        })
        .map(|e| e.key.clone())
        .collect();
    if choice == InstallChoice::AppendOnly && !conflict_keys.is_empty() {
        return Err(UsageError::new(
            "usage-settings-invalid",
            format!(
                "tps-install-conflict: conflict detected — same signal target already present {conflict_keys:?}; \
                 keep-by-default, transfer requires explicit choice=transfer-statistics-target"
            ),
        ));
    }

    // 写入：非冲突键追加/就位；冲突键仅 transfer 时替换（「只更新该统计键」）。
    // 冲突含非字符串旧值（存在即目标，转交 = 用户显式授权替换该键）。
    // 既有回执 = 首次接管事实的权威：重装/升级时沿用，绝不被自有安装值重置（独立复核 R2）。
    let prior = load_receipt(&conn, &record.tool)?;
    let mut receipt_entries = Vec::with_capacity(record.entries.len());
    let mut changed = false;
    for entry in &record.entries {
        let old = env.get(&entry.key).cloned();
        let planned = entry.planned_value.clone();
        // differs 含**形状**：键缺失或非字符串都算不同——规范化串等于计划串的
        // 数字/布尔旧值（`1` vs "1"、`1000` vs "1000"）也必须替换为计划字符串，
        // 否则文件实况与回执 installed_value 不符、遥测可能被静默忽略（Round 2）
        let differs = match env.get(&entry.key) {
            None => true,
            Some(v) => !v.is_string() || v.as_str() != Some(planned.as_str()),
        };
        // 所有权事实：当前值等于**首次回执**记录的安装值 = 我们自己的（含旧版本升级）
        // → 沿用首次接管前的 oldExists/oldValue/指纹；否则（首装，或用户后改被 transfer
        // 显式替换）以当前值为新的接管事实。重装把自有安装值记成「原有值」会让卸载
        // 恢复出我们的安装值而非用户原目标、首次新增键被当原有键保留——这正是 R2。
        let first = prior
            .as_ref()
            .and_then(|r| r.entries.iter().find(|e| e.key == entry.key));
        let ours = first.is_some_and(|f| {
            old.as_ref().and_then(|v| v.as_str()) == Some(f.installed_value.as_str())
        });
        let (old_exists, old_value, old_fingerprint) = match (first, ours) {
            (Some(f), true) => (f.old_exists, f.old_value.clone(), f.old_fingerprint.clone()),
            _ => (
                old.is_some(),
                old.clone(),
                old.as_ref().map(|v| fingerprint(&value_canonical(v))),
            ),
        };
        if differs {
            env.insert(
                entry.key.clone(),
                serde_json::Value::String(planned.clone()),
            );
            changed = true;
        }
        receipt_entries.push(InstallReceiptEntry {
            key: entry.key.clone(),
            old_exists,
            old_value,
            installed_value: planned,
            old_fingerprint,
        });
    }
    if changed {
        write_settings(&settings_path, &root, existed)?;
    }

    let receipt = InstallReceipt {
        tool: record.tool.clone(),
        applied_at_ms: crate::services::usage::now_ms(),
        choice,
        entries: receipt_entries,
        receiver: record.receiver.clone(),
    };
    // 安装记录 + 接收器身份同落 tps_state（记录只进用户私有库；预览记录即焚，防重放）
    let value = serde_json::to_value(&receipt)
        .map_err(|e| UsageError::internal(format!("tps: receipt serialization failed: {e}")))?;
    store::save_state(&conn, &record_state_key(&record.tool), &value)?;
    match &record.receiver {
        Some(receiver) => save_receiver_config(&conn, &record.tool, receiver)?,
        None => unreachable!("claude 路径预览必带接收器身份"),
    }
    store::save_state(
        &conn,
        &preview_state_key(preview_id),
        &serde_json::Value::Null,
    )?;
    // 待应用槽若正指向本预览 → 一并清空（防悬挂指针；指向更新预览时不动它）
    let pending = store::load_state(&conn, &pending_state_key(&record.tool))?;
    if pending
        .and_then(|v| v.as_str().map(str::to_string))
        .as_deref()
        == Some(preview_id)
    {
        store::save_state(
            &conn,
            &pending_state_key(&record.tool),
            &serde_json::Value::Null,
        )?;
    }
    Ok(receipt)
}

/// 卸载自有统计项（Spec §5「卸载仅处理仍匹配安装值的自有项」）：
/// 新增键删除、转交替换键恢复旧统计值、用户后改项**保留**并报冲突；
/// 冲突未清空时保留安装记录（剩余自有项仍可追溯）；从未安装 = 幂等成功。
/// 生产入口；测试用 [`uninstall_at`]。
pub fn uninstall(tool: &str) -> Result<(), UsageError> {
    uninstall_at(&runtime_home(), tool)
}

/// 同 [`uninstall`]，home 注入形态（测试隔离唯一入口）
pub fn uninstall_at(home: &Path, tool: &str) -> Result<(), UsageError> {
    if tool_kind(tool)? == ToolKind::OpenCode {
        return uninstall_opencode_at(home);
    }
    let settings_path = settings_path_for(home, tool)?;
    let conn = open_state_db(home)?;
    // 显式 null = 已卸载/已了结（dao 无删除通道）→ 读侧把 null 视作不存在
    let value = store::load_state(&conn, &record_state_key(tool))?.filter(|v| !v.is_null());
    let Some(value) = value else {
        return Ok(()); // 从未安装：幂等成功
    };
    let receipt: InstallReceipt = serde_json::from_value(value)
        .map_err(|e| UsageError::internal(format!("tps: install record parse failed: {e}")))?;
    if !settings_path.exists() {
        // 设置文件整个被用户删除：无可恢复面，记录一并了结
        store::save_state(&conn, &record_state_key(tool), &serde_json::Value::Null)?;
        return Ok(());
    }
    let (mut root, _existed) = read_settings(&settings_path)?;
    // env 段缺失/被用户改成非对象 = 受管键已全部不在：无可恢复面，不**新建** env
    // （env_obj_mut 的补建语义属于写路径；卸载路径绝不给用户文件添空对象）
    let env_shape_ok = root.get("env").map(|v| v.is_object()).unwrap_or(false);
    if !env_shape_ok {
        store::save_state(&conn, &record_state_key(tool), &serde_json::Value::Null)?;
        return Ok(());
    }
    let env = env_obj_mut(&mut root)?;
    let mut changed = false;
    let mut retained: Vec<String> = Vec::new();
    for entry in &receipt.entries {
        // 取原始 JSON Value 再改 map（避免借用跨过 env 的可变操作）；比对一律
        // Value 逐字相等——字符串形态（安装值恒为字符串）与非字符串恢复值同轨，
        // 数字 1000 与串 "1000" 不混淆（与预览/apply 的形状纪律同一口径）
        let current = env.get(&entry.key).cloned();
        match current {
            // 仍匹配安装值（字符串形态）→ 处置：原先不存在则删除自有项；有旧统计值
            // 则**原样恢复**（old_value 保存原样 JSON 值，非字符串旧值的类型精确还原，
            // 评审 Important 1）
            Some(ref v) if v.as_str() == Some(entry.installed_value.as_str()) => {
                match (&entry.old_exists, &entry.old_value) {
                    (true, Some(old_value)) => {
                        env.insert(entry.key.clone(), old_value.clone());
                    }
                    _ => {
                        env.remove(&entry.key);
                    }
                }
                changed = true;
            }
            // 已恢复（M4 + rider）：现值与记录的原值**逐字相等**（含非字符串旧值的
            // 精确类型——经 :1177 恢复的数字/布尔等原样值同样收敛）= 上次卸载已处理过
            // 本条目（或用户手工改回了原值——值面与恢复目标等价，无剩余动作）。
            // 视为**已完成**：既不保留冲突，也不重复恢复——否则每次重试都把已恢复键
            // 当新的用户改动报冲突，卸载永不收敛、安装记录永久滞留。
            Some(ref v) if entry.old_exists && entry.old_value.as_ref() == Some(v) => {}
            // 键已不在：原先也无 → 早已是目标态；原先有 → 用户删了，不擅自复活旧值
            //（报冲突留给用户决定）
            None if !entry.old_exists => {}
            None => retained.push(entry.key.clone()),
            // 用户后改（含非字符串现值且非恢复值）→ 保留并报冲突
            //（Spec §5「用户后改则保留并报冲突」）
            Some(_) => retained.push(entry.key.clone()),
        }
    }
    if changed {
        write_settings(&settings_path, &root, true)?;
    }
    if retained.is_empty() {
        store::save_state(&conn, &record_state_key(tool), &serde_json::Value::Null)?;
        Ok(())
    } else {
        Err(UsageError::new(
            "usage-settings-invalid",
            format!(
                "tps-uninstall-conflict: keys changed/removed by user, kept per user-modified-first policy: {retained:?}; \
                 remaining owned items cleaned, record kept until conflicts resolved"
            ),
        ))
    }
}

// ==== OpenCode 插件：预览 / 应用 / 卸载（复用 Task 5 所有权机制）====
//
// 与 claude 的 env 键逐一对应的目标语义：自有插件**文件**替代自有统计**键**——
// * preview：只读目标文件现内容（指纹/字节量），绝不写；同一文件名已被占（内容不同）
//   = 冲突，默认保留不覆盖；
// * apply：逐键指纹比对（预览后文件被改 → 拒绝）；append-only 拒绝冲突；transfer
//   显式替换（旧内容存回执，卸载可原样恢复）；
// * uninstall：现内容仍匹配安装值才处置（原先不存在 → 删文件；有旧值 → 原样恢复）；
//   用户后改 → 保留并报冲突；禁止整目录备份覆盖恢复（其他插件文件永不触碰）。

fn preview_opencode_at(home: &Path) -> Result<InstallPreview, UsageError> {
    let target = opencode_plugin_path_for(home);
    let conn = open_state_db(home)?;
    let now = crate::services::usage::now_ms();
    let old = read_own_file(&target)?;

    let planned = OPENCODE_PLUGIN_JS;
    let old_fingerprint = old.as_deref().map(fingerprint);
    // 冲突 = 同名文件已存在且内容 ≠ 计划值（幂等重装 = 非冲突）
    let conflict = matches!(&old_fingerprint, Some(fp) if *fp != fingerprint(planned));
    let conflict_keys = if conflict {
        vec![OPENCODE_PLUGIN_FILE.to_string()]
    } else {
        Vec::new()
    };
    let entries = vec![InstallPreviewEntry {
        key: OPENCODE_PLUGIN_FILE.to_string(),
        old_exists: old.is_some(),
        // 插件 JS 非敏感但无逐字外显必要：预览只给字节量/指纹
        old_masked: old.as_ref().map(|o| {
            format!(
                "[existing plugin file {} bytes, fingerprint {}]",
                o.len(),
                fingerprint(o)
            )
        }),
        old_fingerprint,
        planned_masked: format!(
            "[observer plugin JS {} bytes, fingerprint {}]",
            planned.len(),
            fingerprint(planned)
        ),
        conflict,
    }];
    let persisted = vec![PersistedEntry {
        key: OPENCODE_PLUGIN_FILE.to_string(),
        planned_value: planned.to_string(),
        then_fingerprint: old.as_deref().map(fingerprint),
        then_non_string: false,
        then_value: old.clone(),
    }];

    let preview_id = format!("pv-{:032x}", rand::random::<u128>());
    let record = PersistedPreview {
        preview_id: preview_id.clone(),
        tool: "opencode".into(),
        created_at_ms: now,
        expires_at_ms: now + PREVIEW_TTL_MS,
        receiver: None, // OpenCode 无接收器（插件本地落盘）
        entries: persisted,
    };
    save_preview_record(&conn, &record)?;
    Ok(InstallPreview {
        preview_id,
        tool: "opencode".into(),
        settings_path: target.display().to_string(),
        expires_at_ms: record.expires_at_ms,
        entries,
        conflict_keys,
    })
}

/// 预览记录持久化（与 claude 路径共用即焚槽语义：新预览即焚旧预览，防凭证/计划堆积）
fn save_preview_record(conn: &Connection, record: &PersistedPreview) -> Result<(), UsageError> {
    let value = serde_json::to_value(record)
        .map_err(|e| UsageError::internal(format!("tps: preview serialization failed: {e}")))?;
    let pending_id = store::load_state(conn, &pending_state_key(&record.tool))?
        .and_then(|v| v.as_str().map(str::to_string));
    if let Some(old_id) = pending_id {
        if old_id != record.preview_id {
            store::save_state(conn, &preview_state_key(&old_id), &serde_json::Value::Null)?;
        }
    }
    store::save_state(conn, &preview_state_key(&record.preview_id), &value)?;
    store::save_state(
        conn,
        &pending_state_key(&record.tool),
        &serde_json::json!(record.preview_id),
    )
}

fn apply_opencode_at(
    home: &Path,
    conn: &Connection,
    preview_id: &str,
    record: PersistedPreview,
    choice: InstallChoice,
) -> Result<InstallReceipt, UsageError> {
    let target = opencode_plugin_path_for(home);
    let current = read_own_file(&target)?;

    // 逐键指纹比对：预览后文件漂移（含被删）→ 拒绝，不落盘
    let entry = record.entries.first().ok_or_else(|| {
        UsageError::internal("tps: OpenCode preview record has no entries".to_string())
    })?;
    let current_fp = current.as_deref().map(fingerprint);
    if current_fp != entry.then_fingerprint {
        return Err(UsageError::new(
            "usage-settings-invalid",
            format!(
                "tps-install-preview-stale: file {} changed after preview (current fingerprint {current_fp:?} != preview fingerprint {:?}), refusing to apply; re-run preview",
                target.display(),
                entry.then_fingerprint
            ),
        ));
    }
    // 冲突 = 预览时已有不同内容：append-only 拒绝，transfer 才替换（Spec §5）
    let conflict =
        matches!(&entry.then_fingerprint, Some(then) if *then != fingerprint(&entry.planned_value));
    if conflict && choice == InstallChoice::AppendOnly {
        return Err(UsageError::new(
            "usage-settings-invalid",
            format!(
                "tps-install-conflict: conflict detected — same signal target already present {:?}; \
                 keep-by-default, transfer requires explicit choice=transfer-statistics-target",
                [&entry.key]
            ),
        ));
    }

    let planned = entry.planned_value.clone();
    // 所有权事实（独立复核 R2）：既有回执记录的是**首次接管**。当前内容等于首次回执的
    // 安装值 = 我们自己的文件（含旧版本升级）→ 沿用首次的 oldExists/oldValue/指纹；
    // 绝不因「现内容 == 计划」就把备份重置为空——那会让卸载直接删文件，首次被
    // transfer 替换的用户插件永久丢失。否则：现内容 ≠ 计划 = 被替换的用户内容
    //（首装 transfer，或用户后改被显式 transfer 替换）→ 以当前内容为新接管事实；
    // 现内容缺失 = 新增（卸载时删除）。
    let prior = load_receipt(conn, &record.tool)?;
    let first = prior.as_ref().and_then(|r| r.entries.first());
    let ours = first.is_some_and(|f| current.as_deref() == Some(f.installed_value.as_str()));
    let replaced_user_content = !ours && matches!(current.as_deref(), Some(c) if c != planned);
    let (old_exists, old_value, old_fingerprint) = match (first, ours) {
        (Some(f), true) => (f.old_exists, f.old_value.clone(), f.old_fingerprint.clone()),
        _ => (
            replaced_user_content,
            // 旧文件全文原样保存（transfer 替换后卸载可原样恢复；预览时存于 then_value）
            entry
                .then_value
                .clone()
                .filter(|_| replaced_user_content)
                .map(serde_json::Value::String),
            current.as_deref().map(fingerprint),
        ),
    };
    // 内容与计划不同即写（含自有旧版本升级、用户内容替换）；缺失则新增
    if current.as_deref() != Some(planned.as_str()) {
        write_config_atomic_private(&target, &planned, 0o600)?;
    }
    let receipt = InstallReceipt {
        tool: record.tool.clone(),
        applied_at_ms: crate::services::usage::now_ms(),
        choice,
        entries: vec![InstallReceiptEntry {
            key: entry.key.clone(),
            old_exists,
            old_value,
            installed_value: planned,
            old_fingerprint,
        }],
        receiver: None, // OpenCode 无接收器：插件本地落盘，不经本机 HTTP 面
    };
    let value = serde_json::to_value(&receipt)
        .map_err(|e| UsageError::internal(format!("tps: receipt serialization failed: {e}")))?;
    store::save_state(conn, &record_state_key(&record.tool), &value)?;
    store::save_state(
        conn,
        &preview_state_key(preview_id),
        &serde_json::Value::Null,
    )?;
    // 待应用槽若正指向本预览 → 一并清空（防悬挂指针）
    let pending = store::load_state(conn, &pending_state_key(&record.tool))?;
    if pending
        .and_then(|v| v.as_str().map(str::to_string))
        .as_deref()
        == Some(preview_id)
    {
        store::save_state(
            conn,
            &pending_state_key(&record.tool),
            &serde_json::Value::Null,
        )?;
    }
    Ok(receipt)
}

fn uninstall_opencode_at(home: &Path) -> Result<(), UsageError> {
    let conn = open_state_db(home)?;
    let value = store::load_state(&conn, &record_state_key("opencode"))?.filter(|v| !v.is_null());
    let Some(value) = value else {
        return Ok(()); // 从未安装：幂等成功
    };
    let receipt: InstallReceipt = serde_json::from_value(value)
        .map_err(|e| UsageError::internal(format!("tps: install record parse failed: {e}")))?;
    let target = opencode_plugin_path_for(home);
    let Some(entry) = receipt.entries.first() else {
        store::save_state(
            &conn,
            &record_state_key("opencode"),
            &serde_json::Value::Null,
        )?;
        return Ok(());
    };
    let current = read_own_file(&target)?;
    let Some(current) = current else {
        // 自有文件已被用户删除：无可恢复面，记录一并了结
        store::save_state(
            &conn,
            &record_state_key("opencode"),
            &serde_json::Value::Null,
        )?;
        return Ok(());
    };
    if current != entry.installed_value {
        // 已恢复（M4）：现内容等于记录的安装前旧值（值面或指纹吻合）= 上次卸载已把
        // 自有文件恢复（或等价「配置已写而回执未了结」的可恢复面）→ 视为**卸载完成**
        //（文件不动、记录清除），不得当作用户改动把重试卡死在不收敛的冲突上
        let restored = entry.old_exists
            && (entry.old_fingerprint.as_deref() == Some(fingerprint(&current).as_str())
                || entry.old_value.as_ref().and_then(|v| v.as_str()) == Some(current.as_str()));
        if restored {
            store::save_state(
                &conn,
                &record_state_key("opencode"),
                &serde_json::Value::Null,
            )?;
            return Ok(());
        }
        // 用户后改 → 保留并报冲突（记录保留，剩余自有项仍可追溯）
        return Err(UsageError::new(
            "usage-settings-invalid",
            format!(
                "tps-uninstall-conflict: plugin file {} changed by user, kept per user-modified-first policy; \
                 record kept until conflict resolved",
                target.display()
            ),
        ));
    }
    // 仍匹配安装值 → 处置：原先存在则原样恢复旧内容；原先不存在则删除自有文件
    match (&entry.old_exists, &entry.old_value) {
        (true, Some(serde_json::Value::String(old))) => {
            write_config_atomic_private(&target, old, 0o600)?;
        }
        _ => {
            std::fs::remove_file(&target).map_err(|e| {
                UsageError::new(
                    "usage-source-io",
                    format!(
                        "tps: failed to remove plugin file ({}): {e}",
                        target.display()
                    ),
                )
            })?;
        }
    }
    store::save_state(
        &conn,
        &record_state_key("opencode"),
        &serde_json::Value::Null,
    )?;
    Ok(())
}

/// 生产 home（与 `adapter/claude.rs::base_dir()` 同源的 `dirs::home_dir()`；
/// `*_at` 注入形态是测试唯一入口，本函数不出现在测试路径）
fn runtime_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

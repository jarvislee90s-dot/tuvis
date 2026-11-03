//! TPS 值对象层（Spec §6 公共类型固定的 Rust 镜像）。
//!
//! 命名纪律：本层一律 Rust snake_case；IPC camelCase 镜像由 Task 7/8 的命令层完成
//! （Spec §6「Rust snake_case/IPC camelCase 镜像」）。
//!
//! Option 语义（Spec §6「问号字段响应显式 null」）：响应面的 `?` 字段**不加**
//! `skip_serializing_if`——缺值时必须序列化出显式 `null`，不得省略键。这与
//! `services/usage/model.rs` 的「`?` = 省略」约定**刻意不同**，以本 spec 为准。
//! 「未知不填 0」纪律（Spec §3「失败缺计时为缺失，不填 0」/ §6「工具归属不唯一为
//! null，不填 0」）由 `Option` 承载。
//!
//! 本层只放类型、枚举与 TpsObservation 的**内在**谓词（单条观察自足可判定）；
//! 跨观察的窗口/聚合口径在 policy.rs。

use serde::{Deserialize, Serialize};

// ==== 枚举 ====

/// 计时方法（Spec §3 固定三值 + V2 追加 `LogBoundaryApprox`，序列化串逐字为 kebab-case：
/// `stream-observed` / `client-native` / `native-event-approx` / `log-boundary-approx`）。
/// 不同方法不合并成一条连续精确曲线（Spec §3）——series_key 含 method 即此约束的类型化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimingMethod {
    StreamObserved,
    ClientNative,
    NativeEventApprox,
    /// V2 响应口径（含等待）：来源只有调用级起止（日志边界近似），无生成窗概念
    /// 也不伪造（如 Codex 仅 response，一级标「含等待 · 日志近似」）
    LogBoundaryApprox,
}

impl TimingMethod {
    /// 稳定串形态（与 serde 序列化逐字一致），供日志/键拼接用
    pub fn as_str(&self) -> &'static str {
        match self {
            TimingMethod::StreamObserved => "stream-observed",
            TimingMethod::ClientNative => "client-native",
            TimingMethod::NativeEventApprox => "native-event-approx",
            TimingMethod::LogBoundaryApprox => "log-boundary-approx",
        }
    }
}

/// 观察结果（Spec §3：成功/失败/取消）。失败/取消不入速度，但有可靠起止仍进耗时详情。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Success,
    Failed,
    Cancelled,
}

/// 用途（Spec §3：辅助标题/自动摘要与主任务分别标 purpose，默认不进入主任务曲线）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Purpose {
    /// 主任务
    MainTask,
    /// 辅助标题
    AssistTitle,
    /// 自动摘要
    AutoSummary,
}

/// 输出 Token 与思考的包含关系（Spec §3 末段 + §6 tps_observation「reasoning 包含关系」列）。
/// 这里承载「source 与 provider 的 null 区别」：互斥/已含是**已声明**的两态；
/// provider-reported 是「来源只给了一个输出数、是否含思考未单独声明」；
/// unknown 是**无事实**（如空窗 gap），不得谎称 provider-reported。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputScope {
    /// 互斥：output 不含 reasoning（仅当生成窗覆盖两者时才可相加，Spec §3）
    ExclusiveOutput,
    /// 供应商输出已含思考（不得再相加，Spec §3）
    IncludesReasoning,
    /// provider-reported：思考计数未单独声明；不按字符估思考，不宣称测得隐藏思考
    ProviderReported,
    /// 无事实（gap 点等）
    Unknown,
}

/// 样本质量标记（Spec §3「少量 Token 标小样本」；阈值口径见 policy.rs 常量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TpsQuality {
    Normal,
    SmallSample,
}

/// 点状态（Spec §6：`status(value|gap)`）。空窗保存 gap，不画 0、不沿用旧速度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PointStatus {
    Value,
    Gap,
}

/// 点触发方式（Spec §6：`trigger(startup|scheduled|manual)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PointTrigger {
    Startup,
    Scheduled,
    Manual,
}

// ==== 键与选择器 ====

/// 稳定 series 键（Spec §6「series_key 包含 tool、原始 model/provider、channel、method」）。
/// tool 取观察的 `source`（与 usage 域 `UsageSourceId::db_id()` 同一套小写工具 id 字符串）；
/// model 保留**原始**模型串（不做供应商归并）；channel 无渠道概念时为 null。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesKey {
    pub tool: String,
    pub model: String,
    pub channel: Option<String>,
    pub method: TimingMethod,
}

impl SeriesKey {
    /// 稳定串形态：serde_json 规范化序列化（结构体字段序固定 → 跨进程/跨重启逐字稳定，
    /// 且无分隔符歧义）。用作落库 series_key 列与 BTreeMap 键（迭代序确定）。
    pub fn id(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// 查询选择器（Spec §6 `TpsSelection={toolId?,modelId?,channelKey?,method?}`；None = 不过滤）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TpsSelection {
    pub tool_id: Option<String>,
    pub model_id: Option<String>,
    pub channel_key: Option<String>,
    pub method: Option<TimingMethod>,
}

impl TpsSelection {
    /// 该选择器是否命中某 series（channel 命中 = 选择器与 series 的渠道**同值**）
    pub fn matches_series(&self, k: &SeriesKey) -> bool {
        if let Some(t) = &self.tool_id {
            if &k.tool != t {
                return false;
            }
        }
        if let Some(m) = &self.model_id {
            if &k.model != m {
                return false;
            }
        }
        if let Some(c) = &self.channel_key {
            if k.channel.as_deref() != Some(c.as_str()) {
                return false;
            }
        }
        if let Some(m) = &self.method {
            if &k.method != m {
                return false;
            }
        }
        true
    }
}

// ==== 来源读取契约（Spec §6 ReadBatch；读取层 Task 3–6 消费）====

/// 源游标（Spec §6 tps_state「源游标」列的载体）。value 的具体语义由各来源读取器解释
/// （文件偏移 / 时间水位 / 行号等）；「入参 cursor 只读、失败不推进水位」由读取层落实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceCursor {
    pub source: String,
    pub value: String,
}

impl SourceCursor {
    /// 空/初始游标（尚无水位的来源形态）
    pub fn empty(source: &str) -> Self {
        Self {
            source: source.into(),
            value: String::new(),
        }
    }
}

/// 来源状态（Spec §6「sourceState 始终返回，即使无数据或缺计时」）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceState {
    pub source: String,
    pub cursor: SourceCursor,
    /// 本轮读取是否成功；失败时 cursor 保持原水位
    pub ok: bool,
    /// 来源限制/缺口说明（如 Spec §4 Kimi「SDK 内部重试不可见」、Codex 缺同调用生成窗）
    pub detail: Option<String>,
}

/// 来源读取统一返回（Spec §6 `ReadBatch={observations,sourceState,diagnostics,nextCursor}`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadBatch {
    pub observations: Vec<TpsObservation>,
    pub source_state: SourceState,
    /// 本批诊断（来源限制说明、跳过原因等；不进错误通道）
    pub diagnostics: Vec<String>,
    pub next_cursor: SourceCursor,
}

// ==== 事实（观察）====

/// 一次调用的事实观察（Spec §6 tps_observation 列的 Rust 镜像；
/// 主键 = source + host_instance + stable_call_key，见 [`Self::call_key`]）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsObservation {
    /// 来源 id（与 usage 域同一套小写工具 id 字符串）
    pub source: String,
    /// 宿主实例（同机多实例区分）
    pub host_instance: String,
    /// 来源内稳定调用键（与 source/host 组成主键；点引用它保证「一个点内只一次」）
    pub stable_call_key: String,
    /// 原生开始时刻（epoch ms）
    pub started_at_ms: i64,
    /// 原生首点时刻（epoch ms）；不可得为 null（TTFT 只平均首点可对齐的样本）
    pub first_token_at_ms: Option<i64>,
    /// 原生结束时刻（epoch ms）——窗口判定唯一依据（Spec §2：按**完成**时刻落窗）
    pub ended_at_ms: i64,
    /// 原生整调用时长 ms（可不可得为 null）
    pub native_duration_ms: Option<i64>,
    /// 生成窗时长 ms（首点→结束）；无可靠生成阶段为 null——不得用整请求时间代替（Spec §3）
    pub generation_ms: Option<i64>,
    /// 含等待的响应时长 ms（开始→终态，V2 响应口径 `response = 1000×Σ输出/Σ含等待响应毫秒`
    /// 的分母事实）。旧记录/不可得为 null（serde `default`：旧 JSON 缺键读回 None，序列化恒
    /// 显式 null）；准入口径见 [`Self::response_sample_ms`]——显式值优先，但**绝不修复**
    /// 不一致数据（非正 / 短于同记录生成窗）。
    #[serde(default)]
    pub response_elapsed_ms: Option<i64>,
    /// 输出 Token；不可得为 null（缺 F 的工具跳过，Spec §4）
    pub output_tokens: Option<i64>,
    /// 输出与思考的包含关系（见 [`OutputScope`]）
    pub output_scope: OutputScope,
    /// 本调用内工具调用可靠用时 ms；工具归属不唯一为 null，不填 0（Spec §6）
    pub tool_ms: Option<i64>,
    /// 原始模型串（不归并）
    pub model: String,
    /// 渠道（无渠道概念为 null）
    pub channel: Option<String>,
    /// 会话身份；**只有 session 身份时不称当前回合**（Spec §1.4）
    pub session_key: Option<String>,
    /// 原生回合身份（turn / interaction / trace / conversation-request，Spec §1.4/§6）；
    /// null = 缺原生回合键 → 查询面返回 unknown-turn
    pub turn_key: Option<String>,
    pub purpose: Purpose,
    pub outcome: Outcome,
    pub method: TimingMethod,
    /// 失败原因（outcome=failed/cancelled 时的来源侧说明）
    pub failure_reason: Option<String>,
}

impl TpsObservation {
    /// 表主键三元组（Spec §6：主键 source+host_instance+stable_call_key）
    pub fn call_key(&self) -> (&str, &str, &str) {
        (
            self.source.as_str(),
            self.host_instance.as_str(),
            self.stable_call_key.as_str(),
        )
    }

    /// 是否可入 TPS（Spec §3：请求成功、输出>0、生成时间>0）。
    /// 「计数和时间同次调用」由读取层保证（一行 = 一次调用的同源字段），本谓词锁定数值面。
    pub fn is_tps_valid(&self) -> bool {
        self.outcome == Outcome::Success
            && self.output_tokens.is_some_and(|v| v > 0)
            && self.generation_ms.is_some_and(|v| v > 0)
    }

    /// 响应口径样本时长 ms（V2：`response = 1000×Σ输出/Σ含等待响应毫秒` 的分母面）。
    /// 两条准入路径，**绝不修复不一致数据**：
    /// ① 显式响应窗（`response_elapsed_ms` 有值）：仅当值 > 0 且不短于同记录生成窗
    ///   （同基准一致性；生成窗缺失 = 来源无生成窗概念，如 Codex 日志近似，不阻拦）；
    /// ② 旧记录兼容（无显式响应窗）：仅当旧流式有效性 [`Self::is_tps_valid`] 本就成立
    ///   ——重试/失败/取消/无生成窗的记录不得被响应口径救回——且原生完整时长可靠
    ///   （> 0 且 ≥ 生成窗，同基准检查）。
    pub fn response_sample_ms(&self) -> Option<i64> {
        if let Some(v) = self.response_elapsed_ms {
            return (v > 0 && self.generation_ms.is_none_or(|g| v >= g)).then_some(v);
        }
        if !self.is_tps_valid() {
            return None;
        }
        let d = self.native_duration_ms.filter(|d| *d > 0)?;
        self.generation_ms.is_some_and(|g| d >= g).then_some(d)
    }

    /// 是否可入响应速度（V2）：与 [`Self::is_tps_valid`] 同为「成功 + 正输出 + 对应时长
    /// 可靠」，差别只在时长取响应口径（[`Self::response_sample_ms`]，含等待）而非生成窗。
    /// 失败 / 取消 / 非正 / 短于生成窗的记录一律 false。
    pub fn is_response_valid(&self) -> bool {
        self.response_sample_ms().is_some()
            && self.outcome == Outcome::Success
            && self.output_tokens.is_some_and(|v| v > 0)
    }

    /// 首点延迟 ms（Spec §6「TTFT 只平均成功且首点可对齐的样本」）：
    /// 失败/取消、首点缺失或倒挂（早于开始）→ None。
    pub fn ttft_ms(&self) -> Option<i64> {
        if self.outcome != Outcome::Success {
            return None;
        }
        let ttft = self.first_token_at_ms? - self.started_at_ms;
        (ttft >= 0).then_some(ttft)
    }

    /// 样本模型用时 ms（Spec §3 耗时详情口径：有可靠起止即算，**含失败/取消**；
    /// 两者皆缺 = 缺计时，不填 0）。优先原生整调用时长，其次生成窗。
    pub fn timed_model_ms(&self) -> Option<i64> {
        self.native_duration_ms
            .filter(|v| *v > 0)
            .or_else(|| self.generation_ms.filter(|v| *v > 0))
    }

    /// 样本工具用时 ms（工具归属不唯一为 null，不填 0）
    pub fn timed_tool_ms(&self) -> Option<i64> {
        self.tool_ms.filter(|v| *v > 0)
    }
}

// ==== 聚合面（指标 / 点 / 快照）====

/// 聚合指标（Spec §6 `TpsMetrics` 逐字段镜像；问号字段响应显式 null）。
/// 分类计数（candidate/timedModel/timedTool/failed/cancelled/missingTiming）**不得**
/// 被称作 TPS 有效 n（Spec §3）——TPS 有效 n 只有 `sample_count`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsMetrics {
    /// 样本模型用时累计（含失败/取消的可靠时长；无任何可靠模型用时为 null）
    pub model_ms: Option<i64>,
    /// 样本工具用时累计（工具归属不唯一为 null，不填 0）
    pub tool_ms: Option<i64>,
    /// 平均首点（分母 = ttft_sample_count，独立于 TPS 的 sample_count）
    pub avg_first_token_ms: Option<i64>,
    /// 点内 TPS = 1000×Σ输出/Σ生成毫秒（不平均单请求 TPS）
    pub tps: Option<f64>,
    /// TPS 有效样本数（≤20，Spec §2）
    pub sample_count: i64,
    /// 首点可对齐的成功样本数（独立分母，Spec §6）
    pub ttft_sample_count: i64,
    /// 候选总数（窗口/回合内全部观察，含失败/取消）
    pub candidate_count: i64,
    /// 有可靠模型用时的候选数
    pub timed_model_count: i64,
    /// 有可靠工具用时的候选数
    pub timed_tool_count: i64,
    pub failed_count: i64,
    pub cancelled_count: i64,
    /// 完全缺计时的候选数（失败缺计时为缺失，不填 0）
    pub missing_timing_count: i64,
    /// 最近有效样本结束时刻（epoch ms）
    pub latest_ended_at: Option<i64>,
    /// 距今年龄 ms
    pub age_ms: Option<i64>,
    pub method: TimingMethod,
    pub output_scope: OutputScope,
    pub quality: TpsQuality,
}

/// 采样点（Spec §6：`TpsPoint 包含上述 TpsMetrics 以及 sumOutputTokens/…/status`）——
/// TpsMetrics 的字段**逐字段平铺**在本结构上（IPC 形态同为平铺），另带点自身字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsPoint {
    // ==== TpsMetrics 同名字段（平铺；逐字段一致）====
    pub model_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    pub avg_first_token_ms: Option<i64>,
    pub tps: Option<f64>,
    pub sample_count: i64,
    pub ttft_sample_count: i64,
    pub candidate_count: i64,
    pub timed_model_count: i64,
    pub timed_tool_count: i64,
    pub failed_count: i64,
    pub cancelled_count: i64,
    pub missing_timing_count: i64,
    pub latest_ended_at: Option<i64>,
    pub age_ms: Option<i64>,
    pub method: TimingMethod,
    pub output_scope: OutputScope,
    pub quality: TpsQuality,
    // ==== 点自身字段 ====
    /// 入样有效样本的输出 Token 合计（TPS 分子）
    pub sum_output_tokens: i64,
    /// 入样有效样本的生成窗合计（TPS 分母）
    pub sum_generation_ms: i64,
    /// 首点延迟合计；无可对齐样本为 null（不填 0）
    pub ttft_sum_ms: Option<i64>,
    /// 点 id（= tps_point 表 tick_id；run+采样时刻+series 内唯一）
    pub point_id: String,
    pub series_key: SeriesKey,
    /// 采样时刻（epoch ms，即 at_ms）
    pub sampled_at: i64,
    /// 回看窗长（固定 120000）
    pub window_ms: i64,
    /// 采样段 id（每次 Tuvis 启动新生成；跨关闭期不连线，Spec §2）
    pub run_id: String,
    pub trigger: PointTrigger,
    pub status: PointStatus,
    /// 发点时生效的计划节奏 ms（独立复核 R11）：可见 60000 / 后台 600000——按**发点时的模式**
    /// 记录（startup / manual 点同样带当时模式的节奏）。前端据此判定「缺失的计划采样点」
    ///（相邻点间隔明显超过两端节奏即断线），而不是猜一个通用阈值；None = 旧库行无此信息。
    #[serde(default)]
    pub cadence_ms: Option<i64>,
    /// 入样调用的稳定键（Spec §6 tps_point.sampled_keys 列；最近在前，一个点内一次）
    pub sampled_keys: Vec<String>,
}

impl TpsPoint {
    /// 提取指标面（供 IPC 镜像/快照复用；Task 7/8 消费）
    pub fn metrics(&self) -> TpsMetrics {
        TpsMetrics {
            model_ms: self.model_ms,
            tool_ms: self.tool_ms,
            avg_first_token_ms: self.avg_first_token_ms,
            tps: self.tps,
            sample_count: self.sample_count,
            ttft_sample_count: self.ttft_sample_count,
            candidate_count: self.candidate_count,
            timed_model_count: self.timed_model_count,
            timed_tool_count: self.timed_tool_count,
            failed_count: self.failed_count,
            cancelled_count: self.cancelled_count,
            missing_timing_count: self.missing_timing_count,
            latest_ended_at: self.latest_ended_at,
            age_ms: self.age_ms,
            method: self.method,
            output_scope: self.output_scope,
            quality: self.quality,
        }
    }
}

/// 单 series 快照面（Spec §6 `TpsSeriesSnapshot={seriesKey,lastCall,currentTurn}` +
/// §6 返回条款「latestEndedAt、ageMs 及空值原因」：超 10 分钟不再是当前值时
/// last_call/current_turn 为 null，但年龄与空值原因仍须可见——null 不裸奔）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsSeriesSnapshot {
    pub series_key: SeriesKey,
    /// 最近一次有效完成调用的指标（超 10 分钟 → null，历史点仍可查看）
    pub last_call: Option<TpsMetrics>,
    /// 当前回合（最近有效调用的原生 turnKey）的已采样完成部分；缺原生回合键 → null
    /// （unknown-turn，Spec §6）；只有 session 身份不称当前回合（Spec §1.4）
    pub current_turn: Option<TpsMetrics>,
    /// 最近有效调用结束时刻（即使超 10 分钟仍返回，供年龄展示）
    pub latest_ended_at: Option<i64>,
    /// 距今年龄 ms（同上）
    pub age_ms: Option<i64>,
    /// 空值原因（Spec §6「…及空值原因」）：`stale` / `unknown-turn` / `no-valid-sample`
    pub null_reason: Option<String>,
}

/// 快照（Spec §6 `TpsSnapshot={series,sourceStates}`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsSnapshot {
    pub series: Vec<TpsSeriesSnapshot>,
    pub source_states: Vec<SourceState>,
}

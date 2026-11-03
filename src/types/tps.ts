// TPS 短时抽样公共类型（2026-10-24 计划 Task 8）——Spec §6 固定契约的 TS 镜像。
// Rust snake_case ↔ IPC camelCase：与 src-tauri/src/services/tps/{model,query}.rs 及
// commands/tps.rs 的 serde 形态逐字对齐；**改动必须两端同步**。
//
// Option 纪律：Rust 侧响应面的 `?` 字段**不加** skip_serializing_if（缺值 = 显式 `null`，
// 不省键）——TS 侧一律 `X | null`，消费方以 `=== null` 判「不可得」，不得把 null 当 0。
export type TpsViewRange = "2m" | "10m" | "1h" | "today";

/** 计时方法（Spec §3 固定三值 + V2 追加 log-boundary-approx；不同方法不合并成一条连续精确曲线）。
 *  V2 响应窗事实 `responseElapsedMs` 目前只在后端事实层（TpsObservation，不进 IPC）；
 *  前端消费经 V2 速度查询类型（speed.ts，Task 6/9 引入），本文件暂无观察镜像。 */
export type TpsTimingMethod =
  "stream-observed" | "client-native" | "native-event-approx" | "log-boundary-approx";

/** 输出与思考的包含关系（Spec §3/§6；unknown = 无事实，如空窗 gap）。 */
export type TpsOutputScope =
  "exclusiveOutput" | "includesReasoning" | "providerReported" | "unknown";

/** 样本质量标记（少量 Token 标小样本）。 */
export type TpsQuality = "normal" | "smallSample";

/** 点状态：value = 实测点；gap = 空窗（不画 0、不连线）。 */
export type TpsPointStatus = "value" | "gap";

/** 点触发方式。 */
export type TpsPointTrigger = "startup" | "scheduled" | "manual";

/** 安装选择（kebab-case wire 形态）：默认仅追加；转交统计目标须显式表达。 */
export type TpsInstallChoice = "append-only" | "transfer-statistics-target";

/** 来源状态粗分类——来自 SourceState.detail 的 `tps-status=` 前缀（kebab-case）。
 *  **不得**只看 ok 布尔判状态（合成的 unconfigured 态当前 ok=true）。 */
export type TpsSourceStatusClass =
  "unconfigured" | "await-restart" | "no-sample" | "insufficient-timing" | "read-failed" | "active";

/** 心跳后的调度模式。 */
export type TpsHeartbeatMode = "background" | "foreground" | "disabled";

/** 查询选择器（None = 不过滤；上行 camelCase，缺键 = 不过滤）。 */
export interface TpsSelection {
  toolId?: string | null;
  modelId?: string | null;
  channelKey?: string | null;
  method?: TpsTimingMethod | null;
}

/** 稳定 series 键（tool + 原始 model/provider + channel + method）。 */
export interface TpsSeriesKey {
  tool: string;
  model: string;
  channel: string | null;
  method: TpsTimingMethod;
}

/** 聚合指标（Spec §6 `TpsMetrics` 逐字段镜像）。
 *  分类计数（candidate/timedModel/timedTool/failed/cancelled/missingTiming）是**独立事实**，
 *  不得统称「TPS 有效 n」——TPS 有效 n 只有 sampleCount；TTFT 分母独立为 ttftSampleCount。 */
export interface TpsMetrics {
  modelMs: number | null;
  toolMs: number | null;
  avgFirstTokenMs: number | null;
  tps: number | null;
  sampleCount: number;
  ttftSampleCount: number;
  candidateCount: number;
  timedModelCount: number;
  timedToolCount: number;
  failedCount: number;
  cancelledCount: number;
  missingTimingCount: number;
  latestEndedAt: number | null;
  ageMs: number | null;
  method: TpsTimingMethod;
  outputScope: TpsOutputScope;
  quality: TpsQuality;
}

/** 单 series 快照（Spec §6）：超 10 分钟 lastCall/currentTurn 为显式 null，
 *  但 latestEndedAt / ageMs / nullReason 仍返回（null 不裸奔）。 */
export interface TpsSeriesSnapshot {
  seriesKey: TpsSeriesKey;
  lastCall: TpsMetrics | null;
  currentTurn: TpsMetrics | null;
  latestEndedAt: number | null;
  ageMs: number | null;
  /** `stale` / `unknown-turn` / `no-valid-sample`。 */
  nullReason: string | null;
}

/** 来源状态（detail 前缀 = `tps-status=<class>`，原文全保留在后）。 */
export interface TpsSourceState {
  source: string;
  cursor: { source: string; value: string };
  ok: boolean;
  detail: string | null;
}

/** 快照（Spec §6 `TpsSnapshot={series,sourceStates}`）。 */
export interface TpsSnapshot {
  series: TpsSeriesSnapshot[];
  sourceStates: TpsSourceState[];
}

/** tps_snapshot 载荷：总授权关闭 → disabled=true 且 snapshot 为显式 null（历史不动）。 */
export interface TpsSnapshotResult {
  disabled: boolean;
  snapshot: TpsSnapshot | null;
}

/** 采样点 = TpsMetrics 同名字段平铺 + 点自身字段（Spec §6，IPC 形态同为平铺）。 */
export interface TpsPoint extends TpsMetrics {
  sumOutputTokens: number;
  sumGenerationMs: number;
  ttftSumMs: number | null;
  pointId: string;
  seriesKey: TpsSeriesKey;
  sampledAt: number;
  windowMs: number;
  runId: string;
  trigger: TpsPointTrigger;
  status: TpsPointStatus;
  /** 发点时生效的计划节奏 ms（可见 60000 / 后台 600000；startup/manual 带当时模式节奏）；旧库行 null */
  cadenceMs: number | null;
  sampledKeys: string[];
}

/** tps_points 载荷：knownSeries 供空窗对齐；runId/trigger/status 供前端断线。 */
export interface TpsPointsResult {
  disabled: boolean;
  knownSeries: TpsSeriesKey[];
  points: TpsPoint[];
  rangeSinceMs: number;
}

/** tps_refresh 载荷：限频/单飞让路时 refreshed=false + reason。
 *  **停用态判定只认 snapshot.disabled，不认 reason**（reason 可能是 rate-limited）。 */
export interface TpsRefreshResult {
  refreshed: boolean;
  reason: string | null;
  snapshot: TpsSnapshotResult | null;
}

/** tps_view_heartbeat 载荷。 */
export interface TpsHeartbeatResult {
  mode: TpsHeartbeatMode;
}

/** 安装预览条目（敏感值已脱敏/指纹，不含全文凭证）。 */
export interface TpsPreviewEntry {
  key: string;
  oldExists: boolean;
  oldMasked: string | null;
  oldFingerprint: string | null;
  plannedMasked: string;
  conflict: boolean;
}

/** 安装预览（apply 时比较指纹；预览后再变更会被拒）。 */
export interface TpsInstallPreview {
  previewId: string;
  tool: string;
  settingsPath: string;
  expiresAtMs: number;
  entries: TpsPreviewEntry[];
  conflictKeys: string[];
}

/** 安装回执条目（IPC 面：敏感键 oldValue 置空、installedValue 为 `[已脱敏]`）。 */
export interface TpsReceiptEntry {
  key: string;
  oldExists: boolean;
  oldValue: unknown;
  installedValue: string;
  oldFingerprint: string | null;
}

/** 接收器身份（只带端口；凭证永不上 IPC）。 */
export interface TpsReceiverPublic {
  port: number;
}

/** 安装回执（凭证剥离的 IPC 镜像）。 */
export interface TpsInstallReceipt {
  tool: string;
  appliedAtMs: number;
  choice: TpsInstallChoice;
  entries: TpsReceiptEntry[];
  receiver: TpsReceiverPublic | null;
}

/** 安装族错误信封（code/detail 沿用 UsageError；reason 机器可读分类）。 */
export interface TpsCommandError {
  code: string;
  detail: string;
  reason: string | null;
}

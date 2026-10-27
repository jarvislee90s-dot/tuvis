// TPS 曲线纯层（Task 8）——「点怎么分组、线在哪里断、口径怎么标注」的唯一出处。
// 零 DOM、零 i18n 实例依赖（文案键以常量导出，t() 由组件层完成）。
//
// 断线纪律（Spec §2）：不同 runId、显式 gap、缺失的计划采样点均断线；
// **后台同 runId 稀疏点可以相连**（不能插值生成新实测点）。
// gap 点本身不产生任何坐标点（不画 0、不沿用旧速度），只在图上留下断口。
import { EM_DASH } from "@/lib/usage/format";
import type { TFn } from "@/lib/usage/range";
import type { TpsPoint, TpsSeriesKey, TpsTimingMethod, TpsViewRange } from "@/types/tps";

/** 当前值时效（Spec §2「最新读数超过10分钟不再当当前值」）。 */
export const TPS_STALE_MS = 10 * 60_000;
/** 每点回看窗长（后端固定 120000，Spec §6）。 */
export const TPS_WINDOW_MS = 120_000;

/** view_range 档位与 UI 顺序（Spec §6「view_range仅2m/10m/1h/today」）。 */
export const TPS_VIEW_RANGES: readonly TpsViewRange[] = ["2m", "10m", "1h", "today"];
/** 默认档：近 1 小时（Spec §1.2）。 */
export const TPS_DEFAULT_VIEW_RANGE: TpsViewRange = "1h";

/**
 * 缺失计划点判定的抖动容差（独立复核 R11）：点随身携带**发点时生效的计划节奏** `cadenceMs`
 *（可见 60 s / 后台 600 s；manual / startup 点带当时模式的节奏，由后端调度器写入）。同 run、
 * 无 gap 的相邻实测点若间隔 > 1.5 × max(两端节奏)，说明中间至少一个计划点没有产生 → 断线。
 * 取 **max** 是为了让模式切换不被误判：可见期末点（60 s）之后视图隐藏，下一个点是 600 s 后的
 * 后台点——它自带 600 s 节奏，不是缺点。两端都无节奏信息（旧库行 null）→ 不据此判断，只认
 * gap / runId。这里不再有任何「猜模式」的通用阈值。
 */
export const TPS_CADENCE_TOLERANCE = 1.5;

function missingPlannedBetween(prev: TpsPoint, next: TpsPoint): boolean {
  const cadence = Math.max(prev.cadenceMs ?? 0, next.cadenceMs ?? 0);
  if (cadence <= 0) return false;
  return next.sampledAt - prev.sampledAt > cadence * TPS_CADENCE_TOLERANCE;
}

/** series 的稳定串 id（前端分组用；不落库，落库键由后端 SeriesKey::id() 生成）。 */
export function tpsSeriesId(k: TpsSeriesKey): string {
  return `${k.tool}|${k.model}|${k.channel ?? ""}|${k.method}`;
}

/** 点按 series 分组（保持入参相对顺序 → 组件层先排序再分组即可）。 */
export function groupTpsPointsBySeries(points: TpsPoint[]): Map<string, TpsPoint[]> {
  const out = new Map<string, TpsPoint[]>();
  for (const p of points) {
    const id = tpsSeriesId(p.seriesKey);
    const list = out.get(id);
    if (list) list.push(p);
    else out.set(id, [p]);
  }
  return out;
}

/**
 * 一个 series 内的**连续实测段**。计划连续性只由**计划锚点**（scheduled / startup 点）承载：
 * 按 sampledAt 升序，在 runId 变化、两锚点之间存在 gap 点、或按两端计划节奏判定缺失计划点
 * 处断开。**manual 点是浮点**：调度器的 manual 刷新不重置计划锚点（scheduler 的 last_point_ms
 * 不变），它既不能充当缺失的计划点、也不能把断开的两段桥接起来（独立复核 R11'：
 * `scheduled@0 → manual@50s → scheduled@120s` 必须在 60s 缺锚处断线）。manual 点只附着到
 * 它时间上所属的锚点段（最近一个不晚于它的锚点所在段；若它早于同 run 的全部锚点则附着到
 * 该 run 的第一段；该 run 无锚点则自成一段），保留合法手动插点的显示；附着前对
 * 「段内前点 → 浮点」（尾缘）与「浮点 → 段首点」（前缘，B2）跑与锚点链同段的连续性
 * 检查——不过则自成一段（NB4/B2：不桥接 gap、不跨缺失计划锚点连线）。gap 点不出现在
 * 任何段里（不画 0）。
 */
export function tpsSegments(points: TpsPoint[]): TpsPoint[][] {
  const values = points
    .filter((p) => p.status === "value")
    .sort((a, b) => a.sampledAt - b.sampledAt);
  const gapTimes = points
    .filter((p) => p.status === "gap")
    .map((p) => p.sampledAt)
    .sort((a, b) => a - b);
  const anchors = values.filter((p) => p.trigger !== "manual");
  const floaters = values.filter((p) => p.trigger === "manual");

  // 1) 锚点段：断线判定只看锚点链
  const segments: TpsPoint[][] = [];
  let cur: TpsPoint[] = [];
  for (const p of anchors) {
    if (cur.length > 0) {
      const prev = cur[cur.length - 1]!;
      const gapBetween = gapTimes.some((g) => g > prev.sampledAt && g < p.sampledAt);
      const missingPlanned = !gapBetween && missingPlannedBetween(prev, p);
      if (prev.runId !== p.runId || gapBetween || missingPlanned) {
        segments.push(cur);
        cur = [];
      }
    }
    cur.push(p);
  }
  if (cur.length > 0) segments.push(cur);

  // 2) manual 浮点附着：不改变锚点段的边界。附着前对「段内前点 → 浮点」跑与锚点链
  //    同一段连续性检查（gap / 缺计划锚点，NB4）——不过则浮点自成一段（按时间排序插入
  //    段列表）：浮点不得桥接断口，也不得跨过缺失的计划锚点连线。
  for (const f of floaters) {
    const sameRun = segments.filter((seg) => seg[0]!.runId === f.runId);
    let target: TpsPoint[] | null = null;
    for (const seg of sameRun) {
      if (seg[0]!.sampledAt <= f.sampledAt) target = seg; // 最近一个起点不晚于它的段
    }
    if (!target) target = sameRun[0] ?? null; // 早于全部锚点 → 该 run 第一段
    if (target) {
      let at = target.length;
      while (at > 0 && target[at - 1]!.sampledAt > f.sampledAt) at -= 1;
      const prev = at > 0 ? target[at - 1]! : null;
      // B2：前缘（浮点早于段内全部点）同样要查——「浮点 → 段首点」跑同一连续性检查，
      // 否则前插浮点把显式 gap / 缺失计划锚点桥接成一段。
      const next = at < target.length ? target[at]! : null;
      const broken =
        (prev != null &&
          (gapTimes.some((g) => g > prev.sampledAt && g < f.sampledAt) ||
            missingPlannedBetween(prev, f))) ||
        (prev == null &&
          next != null &&
          (gapTimes.some((g) => g > f.sampledAt && g < next.sampledAt) ||
            missingPlannedBetween(f, next)));
      if (!broken) {
        target.splice(at, 0, f);
        continue;
      }
    }
    segments.push([f]); // 断口/缺锚之后的浮点，或该 run 无锚点：自成一段
  }
  segments.sort((a, b) => a[0]!.sampledAt - b[0]!.sampledAt);
  return segments;
}

/** 计时来源标注键（Spec §3 三方法 + V2 追加方法；en 值保留英文方法名语义）。 */
export const TPS_METHOD_KEYS: Record<TpsTimingMethod, string> = {
  "stream-observed": "usage.tps.method.streamObserved",
  "client-native": "usage.tps.method.clientNative",
  "native-event-approx": "usage.tps.method.nativeEventApprox",
  "log-boundary-approx": "usage.tps.method.logBoundaryApprox",
};

/** 原生事件近似须有「原生近似」标记（Spec §3）。 */
export function isApproxMethod(m: TpsTimingMethod): boolean {
  return m === "native-event-approx";
}

/**
 * 点内 TPS 显示（Spec §2「1000×Σ输出/Σ生成毫秒」，token/s）：≥100 取整，
 * 否则 1 位小数；不可得（null/NaN）为「—」，**绝不显示 0 或 NaN**。
 */
export function formatTpsValue(tps: number | null | undefined): string {
  if (typeof tps !== "number" || !Number.isFinite(tps)) return EM_DASH;
  return tps >= 100 ? String(Math.round(tps)) : tps.toFixed(1);
}

/** 样本累计时长显示：ms / s / m s 三档；不可得为「—」（未知时长为空，不填 0）。 */
export function formatDurationMs(ms: number | null | undefined): string {
  if (typeof ms !== "number" || !Number.isFinite(ms) || ms <= 0) return EM_DASH;
  if (ms < 1_000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1_000).toFixed(1)}s`;
  const m = Math.floor(ms / 60_000);
  const s = Math.round((ms % 60_000) / 1_000);
  return `${m}m ${s}s`;
}

/** 距今年龄（Spec §2「展示…距今年龄」）；不可得为「—」。 */
export function formatAgeMs(ageMs: number | null | undefined, t: TFn): string {
  if (typeof ageMs !== "number" || !Number.isFinite(ageMs) || ageMs < 0) return EM_DASH;
  const sec = Math.round(ageMs / 1_000);
  if (sec < 60) return t("usage.tps.age.sec", { n: sec });
  const min = Math.floor(sec / 60);
  if (min < 60) return t("usage.tps.age.min", { n: min });
  const hour = Math.floor(min / 60);
  if (hour < 24) return t("usage.tps.age.hour", { n: hour });
  return t("usage.tps.age.day", { n: Math.floor(hour / 24) });
}

/** HH:MM x 轴标签（宿主本地时区；不使用 toISOString——那是 UTC 日界）。 */
export function formatTimeLabel(tsMs: number): string {
  const d = new Date(tsMs);
  const p = (v: number) => String(v).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}`;
}

export interface TpsStatusParse {
  status: string | null;
  /** 持久安装事实（独立复核 R10）：可安装来源的 `tps-installed=<bool>` token；无 token → null */
  installed: boolean | null;
  rest: string | null;
}

/**
 * 解析 SourceState.detail 的机器前缀：`tps-status=<class>`（粗分类），其后可选
 * `tps-installed=<true|false>`（来自 tps_state 安装记录的持久事实，不随采样状态变化）。
 * **判来源状态只认这些前缀**，不得只看 ok 布尔（合成的 unconfigured 态当前 ok=true）；
 * **安装/卸载入口只认 installed**，不依赖本组件临时回执或 await-restart 状态。
 * 前缀之后的原文（codex 的 insufficient-generation-timing…、opencode 的 status=…）原样带回。
 */
export function parseTpsStatus(detail: string | null | undefined): TpsStatusParse {
  const d = detail ?? "";
  const prefix = "tps-status=";
  if (!d.startsWith(prefix)) return { status: null, installed: null, rest: d === "" ? null : d };
  let body = d.slice(prefix.length);
  let status: string;
  let sep = body.indexOf("; ");
  if (sep === -1) {
    status = body;
    body = "";
  } else {
    status = body.slice(0, sep);
    body = body.slice(sep + 2);
  }
  let installed: boolean | null = null;
  const installedPrefix = "tps-installed=";
  if (body.startsWith(installedPrefix)) {
    const after = body.slice(installedPrefix.length);
    sep = after.indexOf("; ");
    const token = sep === -1 ? after : after.slice(0, sep);
    if (token === "true" || token === "false") {
      installed = token === "true";
      body = sep === -1 ? "" : after.slice(sep + 2);
    }
  }
  return { status, installed, rest: body === "" ? null : body };
}

// 「已脱敏」以 unicode 转义承载（与后端 `[已脱敏]` 标记逐字对应，避免源码面硬编码中文）
const MASKED_ZH = `\u5df2\u8131\u654f`;

/**
 * 展示面脱敏判定：安装回执的敏感值标记是 `[已脱敏]`（IPC DTO），预览的是
 * `[redacted <指纹>]`——两种标记在展示层同权归一为脱敏 chip，不回显原文标记。
 */
export function isMaskedMarker(v: string): boolean {
  const t = v.trim();
  if (!t.startsWith("[") || !t.endsWith("]")) return false;
  return t.slice(1, -1) === MASKED_ZH || t.slice(1, -1).startsWith("redacted");
}

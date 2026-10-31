// 设置页「用量采集状态」分区（计划① Task 24，用户批准的**最小可见验收面**）。
// 目的：① 结束时就能亲手核对后端数字（手动采集 + 逐源结果 + 今日四桶/命中率），
// 不必等 ② 的看板。**只用已建的 8 条命令，不新增命令。**
// 临时性质：② 上线后本分区可保留为调试入口，也可整段删除——**不要让它长成第二个设置页**
// （分区 id 刻意用 "usageStatus"，把 "usage" 留给 ② 的正式设置分组）。
//
// 口径纪律（本分区是用户唯一能亲手核对后端数字的地方）：
// * **不可得一律空态，绝不回退成 0**（GC 7 / 契约 §2）：`userEst` 可空——`null` 显示「—」，
//   不是「0」；`collectedAt === 0`（从未采集 / 总开关关闭）显示「尚未采集」，不渲染 1970 假时间。
// * **错误码一律过 `KNOWN_USAGE_CODES`**（`usageErrMsg`）：未知码收敛成 `usage.rpc.usage-internal`，
//   **不得**把原始错误串直接当用户文案。逐源失败只能给「泛码」——`UsageSourceStatus` 没有 `detail`
//   字段（契约 §2/W-28，已登记的契约限制），「哪个文件读不了」只在后端日志里。
// * **总开关关闭时**：`usage_dashboard` 早退返回空态（零行/零桶/`collectedAt=0`）、
//   `usage_collect` 返回空源列表。但 **CSV 导出不受该开关门控**（已登记待裁项 B-2）——
//   文案里**不要**写成「关闭后一切都停」。关闭态与「今天没跑」在数字上同形 → 挂载时并发读
//   `usage_get_settings`，`enabled === false` 时给**显式关闭提示**（否则就是把「被关掉」伪装成「没数据」）。
// * **导出/定位那一行是刻意加的**（brief 正文的代码块里没有，见报告「偏差申报」）：
//   `ensure_reveal_allowed` 白名单的**唯一手工验收点**（「系统文件管理器里真的打开了吗」
//   lib 单测覆盖不到）在计划里登记给 Task 24 目验（见 `KNOWN-PLAN-DEFECTS.md` Task 21
//   「reveal 退让的另一半仍归 Task 24 目验」）。只用**既有**命令：`usage_export_csv` +
//   `export_save_text`（契约 §3 的 8 条里的 2 条）与既有 `reveal_dir` 命令（**不在 8 条内**，
//   是 `commands/resource.rs` 的既有命令；Task 21 复用的是白名单核 `ensure_reveal_allowed`，
//   并**没有**调 `reveal_dir`——导出侧首次接线在本任务）——**未新增命令、未新增权限/capability、
//   未新增依赖**。定位与落盘**必须分开报错**（`export.rs` 明文：定位失败不代表导出失败）。
import { SETTINGS_CARD_TITLE } from "@/components/settings/typography";
import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Activity, FolderOpen, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { usageCollectVisible } from "@/lib/api/usage";
import type {
  UsageBuckets,
  UsageCollectResult,
  UsageDashboard,
  UsageSettings,
} from "@/types/usage";

/** 四桶等宽对齐展示（等宽字体 + tabular-nums；命中率一位小数） */
function BucketRow({ label, value }: { label: string; value: number }) {
  return (
    <div className="flex items-baseline justify-between gap-4 py-0.5">
      <span className="text-muted-foreground text-xs">{label}</span>
      <span className="font-mono text-sm tabular-nums">{value.toLocaleString()}</span>
    </div>
  );
}

const BUCKET_KEYS: { key: keyof UsageBuckets; i18n: string }[] = [
  { key: "inputFresh", i18n: "settings.usageStatus.bucketInputFresh" },
  { key: "cacheRead", i18n: "settings.usageStatus.bucketCacheRead" },
  { key: "cacheWrite", i18n: "settings.usageStatus.bucketCacheWrite" },
  { key: "output", i18n: "settings.usageStatus.bucketOutput" },
];

export function UsageStatusSection() {
  const { t } = useAppTranslation();
  const [collecting, setCollecting] = useState(false);
  const [result, setResult] = useState<UsageCollectResult | null>(null);
  const [collectError, setCollectError] = useState<unknown>(null);
  const [today, setToday] = useState<UsageDashboard | null>(null);
  const [loadError, setLoadError] = useState<unknown>(null);
  const [loading, setLoading] = useState(false);
  // 导出与定位（白名单手工验收点）：`exporting` 防重入、`exportedPath` 显示落盘位置
  const [exporting, setExporting] = useState(false);
  const [exportedPath, setExportedPath] = useState<string | null>(null);
  const [exportError, setExportError] = useState<unknown>(null);
  // 定位失败**与导出失败分开**：`export.rs` 明文「定位失败只 warn——文件已落盘，
  // 定位失败不该让导出算失败」（见 `commands/export.rs:178-187` 的 doc）
  const [revealFailed, setRevealFailed] = useState(false);
  // 总开关状态（`usage_get_settings`，冻结的 8 条之内）：**关闭态必须与「今天没跑」区分开**——
  // 关闭时 `usage_dashboard` 早退成空态，与「真的没数据」在数字上完全同形（GC 7 同族的伪装）。
  // 读不到就保持 `null`（**不**猜成「已关闭」：宁可不提示，也不误报用户关掉了）
  const [switchEnabled, setSwitchEnabled] = useState<boolean | null>(null);

  // 今日口径（range=today / groupBy=tool，与浮窗第 1–2 行同口径）
  const loadToday = useCallback(async () => {
    setLoading(true);
    try {
      const d = await invoke<UsageDashboard>("usage_dashboard", {
        range: { preset: "today" },
        groupBy: "tool",
      });
      setToday(d);
      setLoadError(null);
    } catch (e) {
      // 错误态可见可重试：**不得静默回落成空态**（说明书 §P8 状态三态）
      console.error("usage_dashboard failed:", e);
      setLoadError(e);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadToday();
  }, [loadToday]);

  // F7：分区**实际可见**（本组件只在 `visibleIds.has("usageStatus")` 时挂载）即触发一次
  // 可见性采集——「设置页打开即采」，且未打开该分区时一个采集请求都不发。
  //
  // 不复用 `useUsageCollect`（它要求 React Query Provider，而本分区是纯 invoke 的验收面）：
  // 去抖由**后端跨 WebView 的 60s 门闩**统一负责（切换其他分区 / 快速来回都不会触发二次
  // 重型扫描），这里只负责「可见边沿 → 发一次可见性采集」。
  // 手动按钮仍走强制手动命令（`force=true`，总是执行），两者语义互不替代。
  useEffect(() => {
    void usageCollectVisible().catch((e) => {
      // 失败不阻塞本分区渲染：错误态由 dashboard 查询与手动按钮各自呈现（这里只记日志）
      console.warn("[usage] 可见性采集失败：", e);
    });
  }, []);

  // 后端任何**真实采集完成**（可见 / 手动 / ticker / 启动）都会发同一个完成事件：
  // 到达即重读今日口径，让本分区数字与其它 WebView 同步（与 newRecords 无关）。
  // `.catch(() => () => {})`：非 Tauri 环境（浏览器 / Playwright）里 `listen` 会 reject。
  useEffect(() => {
    const un = listen("usage-collected", () => {
      void loadToday();
    }).catch(() => () => {});
    return () => {
      void un.then((unlisten) => unlisten());
    };
  }, [loadToday]);

  // 挂载时并发读总开关（第 5 条命令；只读设置，**不扫描**）
  useEffect(() => {
    let alive = true;
    void (async () => {
      const s = await invoke<UsageSettings | null>("usage_get_settings").catch(() => null);
      // 形状不认识（null / 缺键）一律保持 null：只认显式 `false` 才提示「已关闭」
      if (alive && typeof s?.enabled === "boolean") setSwitchEnabled(s.enabled);
    })();
    return () => {
      alive = false;
    };
  }, []);

  const collect = async () => {
    setCollecting(true);
    setCollectError(null);
    try {
      const r = await invoke<UsageCollectResult>("usage_collect", { force: true });
      setResult(r);
      await loadToday(); // 采集完立刻回读，数字要跟着动
    } catch (e) {
      console.error("usage_collect failed:", e);
      setCollectError(e);
    } finally {
      setCollecting(false);
    }
  };

  // 导出今日口径 CSV 落盘，**再**单独尝试在系统文件管理器中定位（三步同序；落盘路径来自后端
  // 返回值，**不是**前端拼的路径——白名单校验只看后端 canonicalize 后的真实路径）。
  // 两段错误处理**刻意分开**（export.rs 的明文约定）：
  // * 落盘失败 → 「导出失败」+ **不调 reveal_dir**（没有可信路径可开）；
  // * 定位失败 → 只提示「未能自动打开所在目录」，**导出仍然算成功**（路径照常显示）。
  // 文件名过 Rust 白名单 `[A-Za-z0-9._-]` 且以 `.csv` 结尾（只有 `.csv` 会被前置 UTF-8 BOM）。
  const exportCsv = async () => {
    setExporting(true);
    setExportError(null);
    setRevealFailed(false);
    try {
      const csv = await invoke<string>("usage_export_csv", {
        range: { preset: "today" },
        groupBy: "tool",
        filters: {},
      });
      const path = await invoke<string>("export_save_text", {
        name: `mam-usage-today-${Date.now()}.csv`,
        content: csv,
      });
      setExportedPath(path);
      try {
        await invoke("reveal_dir", { path });
      } catch (e) {
        // debug 构建下 `exports_dir()` 认 `TUVIS_HOME`，而白名单只认 `dirs::home_dir()/.tuvis`
        // → 开发机设了 `TUVIS_HOME` 时这里会失败；**不能**因此把整次导出报成失败
        console.error("reveal_dir failed:", e);
        setRevealFailed(true);
      }
    } catch (e) {
      console.error("usage export failed:", e);
      setExportError(e);
    } finally {
      setExporting(false);
    }
  };

  // 空态判据：四桶全 0 且请求次数 0（**不是**「字段为 null」——四桶与 requests 是必填数字）。
  // 命中率的分母 0 与「真的 0% 命中」不可区分（W-08）→ 必须由 `requests === 0` 兜住空态判据。
  const isEmpty =
    !!today &&
    today.totals.requests === 0 &&
    today.totalsBuckets.inputFresh === 0 &&
    today.totalsBuckets.cacheRead === 0 &&
    today.totalsBuckets.cacheWrite === 0 &&
    today.totalsBuckets.output === 0;

  const fmtTime = (ms: number) =>
    ms > 0 ? new Date(ms).toLocaleString() : t("settings.usageStatus.neverCollected");

  return (
    <div className="overflow-hidden rounded-xl border">
      {/* 卡头条（2026-10-11 两级标题拉开）：h2 保留档位常量搬进条内；说明收条右侧 */}
      <div className="bg-muted/40 flex items-center justify-between gap-2 border-b px-4 py-2.5">
        <h2 className={SETTINGS_CARD_TITLE}>{t("settings.usageStatus.title")}</h2>
        <div className="flex min-w-0 items-baseline gap-2">
          <span className="text-muted-foreground truncate text-xs">
            {t("settings.usageStatus.description")}
          </span>
          <Button
            variant="outline"
            size="sm"
            data-testid="usage-status-collect"
            onClick={() => void collect()}
            disabled={collecting}
          >
            <RefreshCw className="mr-1 h-3 w-3" />
            {/* 采集中钩子：只在在途期间带 testid（可测性的唯一抓手） */}
            <span data-testid={collecting ? "usage-status-collecting" : undefined}>
              {collecting
                ? t("settings.usageStatus.collecting")
                : t("settings.usageStatus.collect")}
            </span>
          </Button>
        </div>
      </div>
      <div className="p-3">
        {/* 采集结果：汇总 + 逐源（失败源标红 + 错误码文案） */}
        {collectError ? (
          <p className="mt-3 text-sm text-red-500" data-testid="usage-status-collect-error">
            {t("settings.usageStatus.collectFailed")}：{usageErrMsg(collectError, t)}
          </p>
        ) : null}
        {result ? (
          <div className="mt-3 space-y-2">
            <div
              className="text-muted-foreground flex flex-wrap gap-x-4 text-xs"
              data-testid="usage-status-summary"
            >
              <span>
                {/* 零源轮（总开关关闭）没有可报的截止时间：`collectedAt` 是「now」而**不是**读到的
                  数据代际 → 写「—」，避免给不可得填一个像真的时间 */}
                {t("settings.usageStatus.collectedAt")}：
                {result.sources.length === 0
                  ? t("settings.usageStatus.notAvailable")
                  : fmtTime(result.collectedAt)}
              </span>
              <span>
                {t("settings.usageStatus.duration")}：{result.durationMs} ms
              </span>
              <span>
                {t("settings.usageStatus.newRecords")}：{result.totalNewRecords}
              </span>
            </div>
            {result.sources.length === 0 ? (
              // 总开关关闭时 `usage_collect` 返回空源列表（零扫描）→ 给显式空态，
              // 否则「新增记录 0」会被误读成「采过、但没数据」
              <p className="text-muted-foreground text-xs" data-testid="usage-status-no-sources">
                {t("settings.usageStatus.noSources")}
              </p>
            ) : (
              <div className="divide-border divide-y rounded-md border">
                {result.sources.map((s) => (
                  <div
                    key={s.sourceId}
                    data-testid="usage-source-row"
                    className="flex items-center justify-between gap-3 px-3 py-1.5"
                  >
                    <div className="flex items-center gap-2">
                      <Activity className="text-muted-foreground h-3.5 w-3.5" />
                      <span className="font-mono text-xs">{s.sourceId}</span>
                      {s.ok ? (
                        <span className="rounded bg-emerald-500/10 px-1.5 py-0.5 text-[10px] text-emerald-500">
                          {t("settings.usageStatus.sourceOk")}
                        </span>
                      ) : (
                        <span className="rounded bg-red-500/10 px-1.5 py-0.5 text-[10px] text-red-500">
                          {t("settings.usageStatus.sourceFailed")}
                        </span>
                      )}
                    </div>
                    <div className="text-muted-foreground flex items-center gap-3 text-[11px]">
                      {!s.ok && s.errorCode ? (
                        // 错误码走①的错误码表：usage.rpc.<code>（未知码在 usageErrMsg 内收敛 internal）。
                        // 逐源失败**只有泛码**（契约 §2 的 UsageSourceStatus 无 detail 字段，W-28）
                        <span className="text-red-500">
                          {usageErrMsg({ code: s.errorCode }, t)}
                        </span>
                      ) : null}
                      <span>
                        {t("settings.usageStatus.parsedFiles")} {s.parsedFiles}
                      </span>
                      <span>
                        {t("settings.usageStatus.newRecordsShort")} {s.newRecords}
                      </span>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        ) : null}

        {/* 今日口径：三态（加载 / 错误可重试 / 空态） */}
        <h3 className={`mt-5 ${SETTINGS_CARD_TITLE}`}>{t("settings.usageStatus.todayTitle")}</h3>
        {/* 总开关关闭提示：关闭态 dashboard 早退成空态，与「今天没跑」在数字上完全同形，
          只有这句提示能把两者分开（GC 7：不得把「不可得/被关掉」伪装成合法值） */}
        {switchEnabled === false ? (
          <p className="mt-2 text-sm text-amber-500" data-testid="usage-status-disabled">
            {t("settings.usageStatus.disabledNotice")}
          </p>
        ) : null}
        {loadError ? (
          <div className="mt-2 flex items-center gap-2" data-testid="usage-status-error">
            <p className="text-sm text-red-500">
              {t("settings.usageStatus.loadError")}：{usageErrMsg(loadError, t)}
            </p>
            <Button variant="outline" size="sm" onClick={() => void loadToday()} disabled={loading}>
              {t("settings.usageStatus.retry")}
            </Button>
          </div>
        ) : loading && !today ? (
          <p className="text-muted-foreground mt-2 text-sm">…</p>
        ) : isEmpty ? (
          <p className="text-muted-foreground mt-2 text-sm" data-testid="usage-status-empty">
            {/* 已知关闭时**不**再说「可能今天没跑」（那就是把「被关掉」伪装成「没数据」） */}
            {switchEnabled === false
              ? t("settings.usageStatus.emptyDisabled")
              : t("settings.usageStatus.empty")}
          </p>
        ) : today ? (
          <div className="mt-2 grid gap-x-8 gap-y-1 sm:grid-cols-2">
            <div data-testid="usage-today-buckets">
              {BUCKET_KEYS.map((b) => (
                <BucketRow key={b.key} label={t(b.i18n)} value={today.totalsBuckets[b.key]} />
              ))}
            </div>
            <div>
              <div className="flex items-baseline justify-between gap-4 py-0.5">
                <span className="text-muted-foreground text-xs">
                  {t("settings.usageStatus.hitRate")}
                </span>
                {/* 契约：cacheHitRate 是 0–1 分数 → UI 乘 100 并保留 1 位小数。
                 **W-08**：分母（requests）为 0 时 0.0% 与「真的 0% 命中」不可区分 → 写「—」 */}
                <span className="font-mono text-sm tabular-nums" data-testid="usage-today-hit-rate">
                  {today.totals.requests === 0
                    ? t("settings.usageStatus.notAvailable")
                    : `${(today.totals.cacheHitRate * 100).toFixed(1)}%`}
                </span>
              </div>
              <div className="flex items-baseline justify-between gap-4 py-0.5">
                <span className="text-muted-foreground text-xs">
                  {t("settings.usageStatus.requests")}
                </span>
                <span className="font-mono text-sm tabular-nums">
                  {today.totals.requests.toLocaleString()}
                </span>
              </div>
              {/* GC 7：userEst 可空（zcode/opencode/dsh 恒 null）→ 空态「—」，**绝不显示 0**。
                M4：用 `typeof … === "number"`（而非严格 `=== null`）兜住「上游漏序列化该键
                → `undefined`」的情形，否则 `.toLocaleString()` 会抛、整段分区白屏 */}
              <div
                className="flex items-baseline justify-between gap-4 py-0.5"
                data-testid="usage-today-user-est"
              >
                <span className="text-muted-foreground text-xs">
                  {t("settings.usageStatus.userEst")}
                </span>
                <span className="font-mono text-sm tabular-nums">
                  {typeof today.totals.userEst === "number" && Number.isFinite(today.totals.userEst)
                    ? today.totals.userEst.toLocaleString()
                    : t("settings.usageStatus.notAvailable")}
                </span>
              </div>
              <div className="flex items-baseline justify-between gap-4 py-0.5">
                <span className="text-muted-foreground text-xs">
                  {t("settings.usageStatus.collectedAt")}
                </span>
                <span className="text-xs">{fmtTime(today.collectedAt)}</span>
              </div>
            </div>
          </div>
        ) : null}

        {/* 导出今日 CSV + 打开所在目录：`ensure_reveal_allowed` 白名单的唯一手工验收点
          （必须在**真机**上点，浏览器 mock 的 reveal_dir 是 no-op） */}
        <div className="mt-5 space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              data-testid="usage-status-export"
              onClick={() => void exportCsv()}
              disabled={exporting}
            >
              <FolderOpen className="mr-1 h-3 w-3" />
              {exporting
                ? t("settings.usageStatus.exporting")
                : t("settings.usageStatus.exportAndReveal")}
            </Button>
            {exportedPath ? (
              <span
                className="text-muted-foreground font-mono text-[11px]"
                data-testid="usage-status-export-path"
              >
                {exportedPath}
              </span>
            ) : null}
            {exportError ? (
              <span className="text-sm text-red-500" data-testid="usage-status-export-error">
                {t("settings.usageStatus.exportFailed")}：{usageErrMsg(exportError, t)}
              </span>
            ) : null}
            {/* 定位失败**不是**导出失败（文件已落盘）：独立文案，且路径照常显示 */}
            {revealFailed ? (
              <span className="text-sm text-amber-500" data-testid="usage-status-reveal-error">
                {t("settings.usageStatus.revealFailed", { path: exportedPath ?? "" })}
              </span>
            ) : null}
          </div>
          <p className="text-muted-foreground text-xs">{t("settings.usageStatus.exportHint")}</p>
          {/* **C-3（用户裁决：保持现状 + 补一条前端提示）**：CSV **不受**总开关门控（不改代码），
            但关闭态下必须说清导出的是什么——「**已采集的历史账本**」。否则用户会以为导出的是
            「关闭后的空账本」，或以为要先打开开关才能导出。按钮本身**保留可点**：关掉采集后
            仍要能备份自己的历史账本（那才是真正的数据可用性损失），隐私风险不因开关变化。 */}
          {switchEnabled === false ? (
            <p className="text-xs text-amber-500" data-testid="usage-status-export-disabled-hint">
              {t("settings.usageStatus.exportHintDisabled")}
            </p>
          ) : null}
        </div>
      </div>
    </div>
  );
}

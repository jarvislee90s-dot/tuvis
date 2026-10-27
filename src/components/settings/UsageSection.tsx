// 设置页「用量统计」分区（计划② Task 14）：落地说明书 §P7 的 8 项设置。
//
// 与计划① 的 `UsageStatusSection` **分工**（契约 §5 / 计划 §3 第 43 条）：那一节是「采集状态 + 立即采集」
// 的验收面（id `usageStatus`），本节是**设置面**（id `usage`，图标 `BarChart3`）——8 项里**不含**
// 「采集状态 / 立即采集」，重复实现即越界。
//
// 八项（每项 `onChange` **即时保存**）：
//  1 `enabled`               ui/switch 开关（总开关；后端关闭时零扫描）
//  2 `miniBarRange`          原生 `<select>`：当日 / 近 5 小时 / 近 7 天
//  3 `miniBarToolRows`       `inputMode="numeric"` + 手动过滤数字，夹取 **1–7**（默认 3；上限 7 是
//                            浮窗可用高度的硬约束，与 D20 的固定 5 行共同保证不越界）
//  4 `detailRetentionDays`   同上，夹取 **1–90**；提示明示「N 天前无明细」
//  5 `collectIntervalMin`    同上，夹取 **1–1440**
//  6 `providerMapRules`      **已从界面撤下**（2026-10-07）→ 见文件内注释
//  7 `exportQuote`           **已搬走**（2026-10-07 B1）→ 看板导出条的 `UsageQuoteEditor`
//  8 `exportPose`            **已搬到看板「导出设置」弹层**（2026-10-07）
import { SETTINGS_FIELD } from "@/components/settings/typography";
import { SETTINGS_CARD_TITLE } from "@/components/settings/typography";
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Button } from "@/components/ui/button";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { PricingStatusPanel } from "@/components/usage/PricingStatusPanel";
import { UsageConfigDialog } from "@/components/usage/UsageConfigDialog";
import { SpeedSourcesPanel } from "@/components/speed/SpeedSourcesPanel";
import { USAGE_KEY, useUsageSettingsQuery } from "@/lib/query/queries/usage";
import { useSpeedDashboardQuery } from "@/lib/query/queries/speed";
import { usageSetSettings } from "@/lib/api/usage";
import { useDisplayCurrency, type DisplayCurrency } from "@/lib/usage/displayCurrency";
import { subscribeConfig } from "@/components/pet/petConfig";
import { loadActiveId } from "@/components/pet/petRuntime";
import type { UsageSettings, UsageSettingsPatch } from "@/types/usage";
import type { SpeedDashboardQuery } from "@/types/speed";

/** 浮窗口径三档（顺序即下拉顺序；键族复用 `UsageSettings["miniBarRange"]`，不另立类型） */
const RANGE_OPTIONS: { value: UsageSettings["miniBarRange"]; key: string }[] = [
  { value: "today", key: "settings.usage.rangeToday" },
  { value: "last5h", key: "settings.usage.rangeLast5h" },
  { value: "last7d", key: "settings.usage.rangeLast7d" },
];

/** 行式设置项（左标题 + 提示，右控件）：与设置页其它分区的行距/字号一致；`htmlFor` 关联右列控件 */
function SettingRow({
  label,
  hint,
  htmlFor,
  children,
}: {
  label: string;
  hint?: ReactNode;
  htmlFor?: string;
  children: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-4 py-2.5">
      <div className="flex-1">
        <label className={SETTINGS_FIELD} htmlFor={htmlFor}>
          {label}
        </label>
        {hint ? <p className="text-muted-foreground mt-0.5 text-xs">{hint}</p> : null}
      </div>
      <div className="flex flex-none items-center gap-2">{children}</div>
    </div>
  );
}

function clampDigits(raw: string, min: number, max: number): number | null {
  const digits = raw.replace(/\D/g, "");
  if (digits === "") return null;
  return Math.min(max, Math.max(min, Number(digits)));
}

const TOOL_ROWS_RANGE = [1, 7] as const;

const RETENTION_RANGE = [1, 90] as const;

const INTERVAL_RANGE = [1, 1440] as const;

/**
 * 设置侧取 sourceStates 用的最小 dashboard 查询（Task 11）：设置页不关心曲线/选择，
 * 只用快照里的 `sourceStates`——字段取契约缺省形态（1h / 无选择 / stream）。
 */
const SPEED_SOURCES_QUERY: SpeedDashboardQuery = {
  range: "1h",
  selection: null,
  metricMode: "stream",
  toolId: null,
  search: "",
  sort: "tool",
};

/** 窗口可见性（文档侧）：设置窗隐藏/最小化时面板不再取数（Spec §8.7「面板 open 且窗口可见」）。 */
function useSettingsWindowVisible(): boolean {
  const [visible, setVisible] = useState(
    () => typeof document === "undefined" || document.visibilityState !== "hidden"
  );
  useEffect(() => {
    const sync = () => setVisible(document.visibilityState !== "hidden");
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => document.removeEventListener("visibilitychange", sync);
  }, []);
  return visible;
}

/** 数字设置输入：编辑期保留字符串草稿，失焦/Enter 才夹取并保存，避免逐字符重渲染导致光标跳动。 */
function NumericSettingInput({
  id,
  testId,
  range,
  value,
  className,
  onCommit,
}: {
  id: string;
  testId: string;
  range: readonly [number, number];
  value: number;
  className: string;
  onCommit(next: number): void;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  const commit = () => {
    if (draft === null) return;
    const next = clampDigits(draft, range[0], range[1]);
    setDraft(null);
    if (next !== null && next !== value) onCommit(next);
  };
  return (
    <Input
      id={id}
      data-testid={testId}
      inputMode="numeric"
      className={className}
      value={draft ?? String(value)}
      onChange={(event) => setDraft(event.currentTarget.value.replace(/\D/g, ""))}
      onBlur={commit}
      onKeyDown={(event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          commit();
        }
      }}
    />
  );
}

export function UsageSection() {
  const { t } = useAppTranslation();
  const queryClient = useQueryClient();
  const settingsQuery = useUsageSettingsQuery();
  /** 本地表单态：首次读到设置即落地，之后由**每次写入返回的合并对象**驱动（见 `save`） */
  const [form, setForm] = useState<UsageSettings | null>(null);
  const [saveError, setSaveError] = useState<unknown>(null);
  /** 换宠物回退姿态的一次性提示（用户下次手动选姿态时清掉） */
  const [poseReset, setPoseReset] = useState(false);
  /** 写入序号：只让**最后一次**写入的合并对象回写本地态（连续改动时旧响应不得覆盖新值） */
  const seqRef = useRef(0);
  /** 费用展示币种（USD/CNY）：只影响显示层，不改账本/价格源/导出 */
  const { currency, rate, setCurrency, setRate } = useDisplayCurrency();
  /** 汇率输入草稿：允许用户输入过程中出现无效串，只有合法值才落库 */
  const [rateDraft, setRateDraft] = useState(rate);

  // ── 输出速度来源状态（Task 11 采集管理面板）────────────────────────────────
  // 设置侧纪律（Spec §8.7）：**只在面板 open 且窗口可见时**取 sourceStates，且**单次取数即停**
  // ——不开速度页的 5 秒轮询、不申请视图租约（`useTpsViewLease` 只归速度页宿主）；面板内
  // 刷新/安装/卸载动作失效 `["tps"]` 前缀后经缓存失效订阅再武装一次取数（见下方订阅）。
  // 收口认**终态**：成功与失败都算一次取数完成——失败若不收口，hook 的 refetchInterval 会把
  // 设置面板变成 5 秒轮询循环（2026-10-26 评审 Important 修复），错误由面板内联渲染 + 手动重试。
  const [sourcesOpen, setSourcesOpen] = useState(false);
  const windowVisible = useSettingsWindowVisible();
  /** 单次取数收口：进入终态（isSuccess/isError）即置位，面板关闭时复位（重开再取新事实） */
  const [sourcesSettled, setSourcesSettled] = useState(false);
  /** 已收口的事实代际：成功看 dataUpdatedAt、失败看 errorUpdatedAt——只有**新**完成的取数才
   *  再次收口，刷新/动作触发的重新武装不会被缓存里的旧结果立刻抵消（否则 enabled true→false
   *  抢在重取派发之前） */
  const settledSuRef = useRef<number>(0);
  const settledErRef = useRef<number>(0);
  useEffect(() => {
    if (!sourcesOpen) setSourcesSettled(false);
  }, [sourcesOpen]);
  const speedSourcesQuery = useSpeedDashboardQuery(SPEED_SOURCES_QUERY, {
    active: sourcesOpen && windowVisible && !sourcesSettled,
  });
  useEffect(() => {
    const du = speedSourcesQuery.dataUpdatedAt;
    if (sourcesOpen && du > 0 && du !== settledSuRef.current) {
      settledSuRef.current = du;
      setSourcesSettled(true);
    }
  }, [sourcesOpen, speedSourcesQuery.dataUpdatedAt]);
  useEffect(() => {
    const eu = speedSourcesQuery.errorUpdatedAt;
    if (sourcesOpen && eu > 0 && eu !== settledErRef.current) {
      settledErRef.current = eu;
      setSourcesSettled(true);
    }
  }, [sourcesOpen, speedSourcesQuery.errorUpdatedAt]);
  // 停轮询态（enabled=false）下 invalidate 不会自动重取，且失效不改变 observer 结果
  // （不触发重渲染）→ 订阅缓存失效事件：面板内刷新/安装/卸载动作失效 `["tps"]` 前缀时
  // 重新武装一次取数（关闭状态收到失效也不取——active 仍被 open 压住，重开才取）。
  useEffect(() => {
    const cache = queryClient.getQueryCache();
    return cache.subscribe((event) => {
      if (event.type !== "updated" || event.action.type !== "invalidate") return;
      if (event.query.queryKey[0] !== "tps") return;
      setSourcesSettled(false);
    });
  }, [queryClient]);

  useEffect(() => {
    setRateDraft(rate);
  }, [rate]);

  useEffect(() => {
    // 一次性落地：之后本地态由写入返回的合并对象驱动，查询重取（失效后）不再覆盖用户正在编辑的值
    if (settingsQuery.data && form === null) setForm(settingsQuery.data);
  }, [settingsQuery.data, form]);

  const save = useCallback(
    async (patch: UsageSettingsPatch) => {
      const seq = ++seqRef.current;
      setSaveError(null);
      try {
        const merged = await usageSetSettings(patch);
        // 「以它回写本地态」：后端返回的是合并后的**完整 8 字段**，本地 patch 合并会丢别处的改动
        if (seq === seqRef.current) setForm(merged);
        await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
      } catch (e) {
        // 整包拒绝**不静默**：任何一次失败都显示原因（不做「旧响应丢弃」——吞掉失败比多显示一条过期
        // 错误更糟；下一次尝试开头会先清掉它）。原因走 usage.rpc.<code> 码表 + detail 通道。
        console.error("usageSetSettings failed:", e);
        setSaveError(e);
      }
    },
    [queryClient]
  );

  /** 即时保存：先乐观置位（受控控件不能等 IPC 往返），再让合并对象收口 */
  const change = (patch: UsageSettingsPatch) => {
    setForm((f) => (f ? { ...f, ...patch } : f));
    void save(patch);
  };

  // 换宠物 → 姿态回退 `random`（见文件头第三条纪律）。`subscribeConfig` 覆盖同窗口与跨窗口两条路径；
  // 只有**激活指针真的变了**才算换宠物（显隐 / 大小 / 置顶 / 音效等配置变更不触发）。
  const pose = form?.exportPose;
  useEffect(() => {
    let lastId = loadActiveId();
    return subscribeConfig(() => {
      const nextId = loadActiveId();
      if (nextId === lastId) return;
      lastId = nextId;
      if (pose === undefined || pose === "random") return; // 已是随机：不重复提交、也不打扰
      setPoseReset(true);
      setForm((f) => (f ? { ...f, exportPose: "random" } : f));
      void save({ exportPose: "random" });
    });
  }, [pose, save]);

  return (
    <div className="overflow-hidden rounded-xl border">
      {/* 卡头条（2026-10-11 两级标题拉开）：h2 保留档位常量搬进条内；说明收条右侧 */}
      <div className="bg-muted/40 flex items-baseline justify-between gap-2 border-b px-4 py-2.5">
        <h2 className={SETTINGS_CARD_TITLE}>{t("settings.usage.title")}</h2>
        <span className="text-muted-foreground truncate text-xs">{t("settings.usage.desc")}</span>
      </div>
      <div className="p-3">
        {/* 保存失败：命令级整包拒绝（越界 / 形态不符 / 落库失败）→ 原因必须可见可重试（不静默） */}
        {saveError ? (
          <p className="mt-2 text-sm text-red-500" data-testid="usage-set-error">
            {t("settings.usage.saveFailed")}：{usageErrMsg(saveError, t)}
          </p>
        ) : null}

        {/* 三态：读失败（可见可重试） / 加载中 / 表单。已有表单时不再被「失效后重取的瞬时失败」清空
          （写入自己的失败另有 usage-set-error 显示） */}
        {!form ? (
          settingsQuery.isError ? (
            <div className="mt-2 flex items-center gap-2" data-testid="usage-load-error">
              <p className="text-sm text-red-500">{usageErrMsg(settingsQuery.error, t)}</p>
              <Button variant="outline" size="sm" onClick={() => void settingsQuery.refetch()}>
                {t("common.retry")}
              </Button>
            </div>
          ) : (
            <p className="text-muted-foreground mt-2 text-sm">{t("common.loading")}</p>
          )
        ) : (
          <div className="mt-2 space-y-0">
            <SettingRow
              label={t("settings.usage.enabled")}
              hint={t("settings.usage.enabledHint")}
              htmlFor="usage-enabled"
            >
              <Switch
                id="usage-enabled"
                data-testid="usage-enabled"
                checked={form.enabled}
                onCheckedChange={(v) => change({ enabled: v })}
              />
            </SettingRow>
            <div className="border-t" />

            <SettingRow label={t("settings.usage.miniBarRange")} htmlFor="usage-minibar-range">
              <select
                id="usage-minibar-range"
                data-testid="usage-minibar-range"
                className="bg-background h-8 rounded-md border px-2 text-sm"
                value={form.miniBarRange}
                onChange={(e) =>
                  change({ miniBarRange: e.currentTarget.value as UsageSettings["miniBarRange"] })
                }
              >
                {RANGE_OPTIONS.map((o) => (
                  <option key={o.value} value={o.value}>
                    {t(o.key)}
                  </option>
                ))}
              </select>
            </SettingRow>
            <div className="border-t" />

            <SettingRow
              label={t("settings.usage.miniBarToolRows")}
              hint={t("settings.usage.miniBarToolRowsHint")}
              htmlFor="usage-minibar-tool-rows"
            >
              <NumericSettingInput
                id="usage-minibar-tool-rows"
                testId="usage-minibar-tool-rows"
                range={TOOL_ROWS_RANGE}
                className="h-8 w-16 text-right font-mono text-sm tabular-nums"
                value={form.miniBarToolRows}
                onCommit={(next) => change({ miniBarToolRows: next })}
              />
            </SettingRow>
            <div className="border-t" />

            <SettingRow
              label={t("settings.usage.detailRetentionDays")}
              hint={
                // testid：提示必须随值（且随夹取）实时更新 —— 用例 6 的唯一抓手
                <span data-testid="usage-detail-retention-hint">
                  {t("settings.usage.detailRetentionHint", { days: form.detailRetentionDays })}
                </span>
              }
              htmlFor="usage-detail-retention-days"
            >
              <NumericSettingInput
                id="usage-detail-retention-days"
                testId="usage-detail-retention-days"
                range={RETENTION_RANGE}
                className="h-8 w-20 text-right font-mono text-sm tabular-nums"
                value={form.detailRetentionDays}
                onCommit={(next) => change({ detailRetentionDays: next })}
              />
            </SettingRow>
            <div className="border-t" />

            <SettingRow
              label={t("settings.usage.collectIntervalMin")}
              hint={t("settings.usage.collectIntervalHint")}
              htmlFor="usage-collect-interval-min"
            >
              <NumericSettingInput
                id="usage-collect-interval-min"
                testId="usage-collect-interval-min"
                range={INTERVAL_RANGE}
                className="h-8 w-20 text-right font-mono text-sm tabular-nums"
                value={form.collectIntervalMin}
                onCommit={(next) => change({ collectIntervalMin: next })}
              />
            </SettingRow>
            <div className="border-t" />

            {/* 显示币种（V2 手工测试反馈）：只影响费用展示；价格源与 CSV 仍为 USD */}
            <SettingRow
              label={t("settings.usage.displayCurrency")}
              hint={t("settings.usage.displayCurrencyHint")}
              htmlFor="usage-display-currency"
            >
              <select
                id="usage-display-currency"
                data-testid="usage-display-currency"
                className="bg-background h-8 rounded-md border px-2 text-sm"
                value={currency}
                onChange={(e) => setCurrency(e.currentTarget.value as DisplayCurrency)}
              >
                <option value="usd">USD</option>
                <option value="cny">CNY</option>
              </select>
            </SettingRow>
            {currency === "cny" ? (
              <>
                <div className="border-t" />
                <SettingRow
                  label={t("settings.usage.usdCnyRate")}
                  hint={t("settings.usage.usdCnyRateHint")}
                  htmlFor="usage-usd-cny-rate"
                >
                  <Input
                    id="usage-usd-cny-rate"
                    data-testid="usage-usd-cny-rate"
                    inputMode="decimal"
                    className="h-8 w-24 text-right font-mono text-sm tabular-nums"
                    value={rateDraft}
                    onChange={(e) => {
                      setRateDraft(e.currentTarget.value);
                      setRate(e.currentTarget.value);
                    }}
                    onBlur={() => setRateDraft(rate)}
                  />
                </SettingRow>
              </>
            ) : null}
            <div className="border-t" />

            {/* 显示与计价配置（2026-10-20 第三轮修订）：设置页与 D 卡共用一个弹窗；
              弹窗内上半是模型条目，下半是可折叠的高级覆盖（只保留四价覆盖）。 */}
            <div className="flex items-center justify-between gap-4 py-2.5">
              <div className="min-w-0 flex-1">
                <p className={SETTINGS_FIELD}>{t("usage.config.title")}</p>
                <p className="text-muted-foreground mt-0.5 text-xs">{t("usage.config.desc")}</p>
              </div>
              <UsageConfigDialog
                t={t}
                instanceId="settings"
                trigger={
                  <Button size="sm" variant="outline" data-testid="usage-config-open-settings">
                    {t("usage.config.open")}
                  </Button>
                }
              />
            </div>
            <div className="border-t" />

            {/* 定价目录状态（Task 17）：与「计费渠道与规则」并列；开关/刷新都需用户显式操作 */}
            <div className="space-y-1 py-2.5">
              <p className={SETTINGS_FIELD}>{t("settings.usage.pricingTitle")}</p>
              <p className="text-muted-foreground text-xs">{t("settings.usage.pricingHint")}</p>
              <PricingStatusPanel t={t} />
            </div>
            <div className="border-t" />

            {/* 输出速度采集管理（Task 11）：设置与速度页共用同一个 SpeedSourcesPanel。
              取数只在面板 open 且窗口可见时发生（单次，见上方状态机）；安装/卸载动作与
              速度页走同一套 TpsInstallFlow，不在此复制安装算法。 */}
            <div className="flex items-center justify-between gap-4 py-2.5">
              <div className="min-w-0 flex-1">
                <p className={SETTINGS_FIELD}>{t("settings.usage.sourcesTitle")}</p>
                <p className="text-muted-foreground mt-0.5 text-xs">
                  {t("settings.usage.sourcesHint")}
                </p>
              </div>
              <Button
                size="sm"
                variant="outline"
                data-testid="usage-sources-open"
                onClick={() => setSourcesOpen(true)}
              >
                {t("settings.usage.sourcesOpen")}
              </Button>
            </div>
            <SpeedSourcesPanel
              open={sourcesOpen}
              onOpenChange={setSourcesOpen}
              sources={speedSourcesQuery.data?.sourceStates ?? []}
              fetchState={
                speedSourcesQuery.isError
                  ? "error"
                  : speedSourcesQuery.isPending && sourcesOpen
                    ? "loading"
                    : "ready"
              }
              onRetry={() => setSourcesSettled(false)}
            />
            <div className="border-t" />

            {/* 供应商映射规则（JSON）**已从界面撤下**（2026-10-07 用户裁决）：
              用户原话「这个是啥意思呀？我都不太懂。要不就不提供给用户编辑了」。
              它要求用户手写 `{"rules":[{"prefix":…,"provider":…}]}` 这种形态，是**开发者口径**的东西，
              摆在设置页里既看不懂、又容易写坏（写坏了还得靠后端整包拒绝才发现）。
              字段 `providerMapRules` 与后端解析**原样保留**（`provider.rs` 照旧读它；`UsageSettings` 里也还在），
              只是不再提供手工编辑入口 —— 将来要开放，应做成「前缀 / 供应商」两列的可视化表格，
              而不是让人手写 JSON。 */}

            {/* 分享图评语**不在本页**（2026-10-07 用户裁决 B1）：它搬到了「导出所在的地方」——
              用量看板吸顶导出条上的「评语」按钮（`components/usage/UsageQuoteEditor.tsx`）。
              字段本身仍是 `UsageSettings.exportQuote`、仍走同一条 `usage_set_settings`。 */}

            {/* 分享图姿态**不在本页**（2026-10-07 用户裁决）：与评语同理搬到了「导出所在的地方」——
              用量看板导出条上的「导出设置」弹层（`components/usage/UsageExportSettings.tsx`）。
              字段仍是 `UsageSettings.exportPose`、仍走同一条 `usage_set_settings`。 */}

            {/* 换宠物回退提示（只在真的发生过回退时出现） */}
            {poseReset ? (
              <p className="text-xs text-amber-500" data-testid="usage-pose-reset">
                {t("settings.usage.poseResetOnSwitch")}
                {/* 姿态选择器已搬到看板「导出设置」⇒ 提示里必须点明去哪儿改（否则用户在这页找不到它） */}
              </p>
            ) : null}
          </div>
        )}
      </div>
    </div>
  );
}

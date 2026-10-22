// 自定义价格规则管理（2026-10-21：固定价 / 分时价）。
//
// 口径：
//  * 规则按归一化模型 ID 全局生效；价目表刷新不会覆盖用户值；
//  * 固定价 = 现在的四价覆盖；分时价 = 高峰四价 + 低谷四价 + 周一至周五时段；
//  * 四价用文本输入，提交字符串，空串 → null；至少一项非空；非法数字前端拒绝；
//  * 分时窗口只支持整点，允许跨午夜；低谷缺项回落目录价（后端合并）；
//  * 保存/删除后统一 `invalidateQueries({queryKey: USAGE_KEY})`，覆盖价与费用查询一起失效；
//  * 不新增轮询、不改 locale。
import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { UsageInfoTip } from "@/components/usage/UsageInfoTip";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { usagePricingOverrideDelete, usagePricingOverrideSet } from "@/lib/api/usageV2";
import { USAGE_KEY, useUsagePriceOverridesQuery } from "@/lib/query/queries/usageV2";
import { EM_DASH, fmtDateTime } from "@/lib/usage/format";
import { useDisplayCurrency } from "@/lib/usage/displayCurrency";
import type { TFn } from "@/lib/usage/range";
import type {
  PriceCurrency,
  PriceOverrideView,
  TimePriceRule,
  TimePriceTimezone,
  TimePriceWindow,
} from "@/types/usageV2";

/** 条目快捷入口的预填请求；nonce 保证重复点击同一模型也重新打开 */
export interface PriceOverridePrefill {
  /** 原始模型名（表单显示用，后端保存时再归一化） */
  modelId: string;
  /** 后端归一化键（查找已有覆盖 / 徽标用） */
  normalizedModelId: string;
  nonce: number;
}

export interface PriceOverrideManagerProps {
  t: TFn;
  /** 外部请求预填并打开表单；已有同键覆盖时进入编辑态 */
  prefill?: PriceOverridePrefill | null;
  onPrefillHandled?(): void;
}

type RateKey = "input" | "cacheRead" | "cacheWrite" | "output";

/** 四价输入顺序与字段名（提交键与视图键一致）。 */
const RATE_FIELDS: Array<{ key: RateKey; labelKey: string; testId: string }> = [
  { key: "input", labelKey: "usage.pricing.overrides.input", testId: "price-override-input" },
  {
    key: "cacheRead",
    labelKey: "usage.pricing.overrides.cacheRead",
    testId: "price-override-cache-read",
  },
  {
    key: "cacheWrite",
    labelKey: "usage.pricing.overrides.cacheWrite",
    testId: "price-override-cache-write",
  },
  { key: "output", labelKey: "usage.pricing.overrides.output", testId: "price-override-output" },
];

type RateDraft = Record<RateKey, string>;

const EMPTY_DRAFT: RateDraft = { input: "", cacheRead: "", cacheWrite: "", output: "" };

/** 与后端 `parse_rate` / mock 同口径：非负十进制、最多 9 位小数（不接受 `.5` / `1e3`）。 */
const RATE_RE = /^\d+(?:\.\d{1,9})?$/;

/** 空串（含纯空白）→ null；否则原样提交（已 trim）。 */
function toRate(v: string): string | null {
  const trimmed = v.trim();
  return trimmed === "" ? null : trimmed;
}

function isRateValid(v: string): boolean {
  const trimmed = v.trim();
  return trimmed === "" || RATE_RE.test(trimmed);
}

/** 整点选项（分时窗口只支持 HH:00；00:00 同时表示午夜/结束）。 */
const HOUR_OPTIONS = Array.from({ length: 24 }, (_, hour) => `${String(hour).padStart(2, "0")}:00`);

const TIMEZONE_OPTIONS: { value: TimePriceTimezone; labelKey: string }[] = [
  { value: "asiaShanghai", labelKey: "usage.pricing.overrides.timezoneAsiaShanghai" },
  { value: "utc", labelKey: "usage.pricing.overrides.timezoneUtc" },
  { value: "local", labelKey: "usage.pricing.overrides.timezoneLocal" },
];

/** 按百分比缩放一个十进制费率文本；非法/空值保持原样。 */
function scaleRateText(value: string, percent: number): string {
  const trimmed = value.trim();
  if (trimmed === "") return "";
  const number = Number(trimmed);
  if (!Number.isFinite(number)) return value;
  const scaled = (number * percent) / 100;
  if (scaled === 0) return "0";
  return scaled.toFixed(9).replace(/0+$/, "").replace(/\.$/, "");
}

function timezoneLabel(t: TFn, timezone: TimePriceTimezone): string {
  const key = TIMEZONE_OPTIONS.find((item) => item.value === timezone)?.labelKey;
  return key ? t(key) : timezone;
}

function windowsLabel(windows: readonly TimePriceWindow[]): string {
  return windows.map((window) => `${window.start}–${window.end}`).join(", ");
}

function offpeakRateOf(rule: TimePriceRule | null | undefined, key: RateKey): string | null {
  if (!rule) return null;
  if (key === "input") return rule.offpeakInput;
  if (key === "cacheRead") return rule.offpeakCacheRead;
  if (key === "cacheWrite") return rule.offpeakCacheWrite;
  return rule.offpeakOutput;
}

/** 分钟边界；跨午夜拆成当天两段，用于与后端同口径的重叠校验。 */
function windowSegments(windows: readonly TimePriceWindow[]): [number, number][] {
  const minutes = (value: string): number => {
    const [hour, minute] = value.split(":").map(Number);
    return hour * 60 + minute;
  };
  return windows.flatMap((window) => {
    const start = minutes(window.start);
    const end = minutes(window.end);
    return start < end
      ? ([[start, end]] as [number, number][])
      : ([
          [start, 1440],
          [0, end],
        ] as [number, number][]);
  });
}

function windowsOverlap(windows: readonly TimePriceWindow[]): boolean {
  const segments = windowSegments(windows);
  return segments.some((left, index) =>
    segments.slice(index + 1).some((right) => left[0] < right[1] && right[0] < left[1])
  );
}

export function PriceOverrideManager({ t, prefill, onPrefillHandled }: PriceOverrideManagerProps) {
  const queryClient = useQueryClient();
  const overrides = useUsagePriceOverridesQuery();
  const { currency: displayCurrency } = useDisplayCurrency();
  // 兼容宿主 mock 未登记该命令时返回 undefined：按空列表降级，不炸整块设置页
  const rows = overrides.data ?? [];

  const [formOpen, setFormOpen] = useState(false);
  /** 编辑中的归一化模型 ID；null = 新增。模型键是主键，编辑时只读。 */
  const [editingId, setEditingId] = useState<string | null>(null);
  const [model, setModel] = useState("");
  const [currency, setCurrency] = useState<PriceCurrency>("USD");
  const [rates, setRates] = useState<RateDraft>(EMPTY_DRAFT);
  const [timeEnabled, setTimeEnabled] = useState(false);
  const [timezone, setTimezone] = useState<TimePriceTimezone>("asiaShanghai");
  const [windows, setWindows] = useState<TimePriceWindow[]>([{ start: "09:00", end: "12:00" }]);
  const [offpeakRates, setOffpeakRates] = useState<RateDraft>(EMPTY_DRAFT);
  const [discountPercent, setDiscountPercent] = useState("50");
  const [formError, setFormError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<unknown>(null);
  const [notice, setNotice] = useState(false);
  const [busy, setBusy] = useState(false);
  const [confirmingId, setConfirmingId] = useState<string | null>(null);
  const confirmRef = useRef<HTMLButtonElement>(null);

  // 进入确认态后把键盘焦点移到确认按钮（与别名管理器同一可访问性纪律）
  useEffect(() => {
    if (confirmingId !== null) confirmRef.current?.focus();
  }, [confirmingId]);

  const resetForm = () => {
    setFormOpen(false);
    setEditingId(null);
    setModel("");
    setCurrency("USD");
    setRates(EMPTY_DRAFT);
    setTimeEnabled(false);
    setTimezone("asiaShanghai");
    setWindows([{ start: "09:00", end: "12:00" }]);
    setOffpeakRates(EMPTY_DRAFT);
    setDiscountPercent("50");
    setFormError(null);
    setActionError(null);
  };

  const beginAdd = (initialModel = "") => {
    resetForm();
    // 新覆盖价默认使用当前显示币种（用户仍可切换）。
    setCurrency(displayCurrency === "cny" ? "CNY" : "USD");
    setModel(initialModel);
    setNotice(false);
    setFormOpen(true);
  };

  const beginEdit = (view: PriceOverrideView) => {
    setEditingId(view.normalizedModelId);
    setModel(view.normalizedModelId);
    setCurrency(view.currency);
    setRates({
      input: view.input ?? "",
      cacheRead: view.cacheRead ?? "",
      cacheWrite: view.cacheWrite ?? "",
      output: view.output ?? "",
    });
    setTimeEnabled(view.timeRule != null);
    setTimezone(view.timeRule?.timezone ?? "asiaShanghai");
    setWindows(
      view.timeRule?.windows?.length ? view.timeRule.windows : [{ start: "09:00", end: "12:00" }]
    );
    setOffpeakRates({
      input: view.timeRule?.offpeakInput ?? "",
      cacheRead: view.timeRule?.offpeakCacheRead ?? "",
      cacheWrite: view.timeRule?.offpeakCacheWrite ?? "",
      output: view.timeRule?.offpeakOutput ?? "",
    });
    setDiscountPercent("50");
    setFormError(null);
    setActionError(null);
    setNotice(false);
    setConfirmingId(null);
    setFormOpen(true);
  };

  // 条目快捷入口：等覆盖列表就绪后，已有同键 → 编辑；否则新增并预填原始模型名。
  const handledPrefill = useRef<number | null>(null);
  useEffect(() => {
    if (!prefill || overrides.isPending || handledPrefill.current === prefill.nonce) return;
    handledPrefill.current = prefill.nonce;
    const existing = rows.find((view) => view.normalizedModelId === prefill.normalizedModelId);
    if (existing) {
      beginEdit(existing);
    } else {
      beginAdd(prefill.modelId);
    }
    onPrefillHandled?.();
    // 只在 nonce 变化/列表就绪时消费一次；handledPrefill 防止函数依赖重跑。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [prefill, rows, overrides.isPending, displayCurrency]);

  const setRate = (key: RateKey, value: string) => {
    setRates((prev) => ({ ...prev, [key]: value }));
    setFormError(null);
  };

  const setOffpeakRate = (key: RateKey, value: string) => {
    setOffpeakRates((prev) => ({ ...prev, [key]: value }));
    setFormError(null);
  };

  const applyDiscount = () => {
    const percent = Number(discountPercent);
    if (!Number.isFinite(percent) || percent <= 0 || percent > 100) {
      setFormError(t("usage.pricing.overrides.invalidDiscount"));
      return;
    }
    setOffpeakRates({
      input: scaleRateText(rates.input, percent),
      cacheRead: scaleRateText(rates.cacheRead, percent),
      cacheWrite: scaleRateText(rates.cacheWrite, percent),
      output: scaleRateText(rates.output, percent),
    });
    setFormError(null);
  };

  const updateWindow = (index: number, patch: Partial<TimePriceWindow>) => {
    setWindows((prev) => prev.map((window, i) => (i === index ? { ...window, ...patch } : window)));
    setFormError(null);
  };

  const submit = async () => {
    setFormError(null);
    setActionError(null);
    const normalizedModelId = model.trim();
    if (normalizedModelId === "") return;

    const values = RATE_FIELDS.map(({ key }) => rates[key]);
    if (values.some((value) => !isRateValid(value))) {
      setFormError(t("usage.pricing.overrides.invalidRate"));
      return;
    }
    if (values.every((value) => value.trim() === "")) {
      setFormError(t("usage.pricing.overrides.atLeastOne"));
      return;
    }

    let timeRule: TimePriceRule | null = null;
    if (timeEnabled) {
      if (windows.length === 0) {
        setFormError(t("usage.pricing.overrides.atLeastOneWindow"));
        return;
      }
      if (windows.some((window) => window.start === window.end)) {
        setFormError(t("usage.pricing.overrides.windowInvalid"));
        return;
      }
      if (windowsOverlap(windows)) {
        setFormError(t("usage.pricing.overrides.windowOverlap"));
        return;
      }
      const offpeakValues = RATE_FIELDS.map(({ key }) => offpeakRates[key]);
      if (offpeakValues.some((value) => !isRateValid(value))) {
        setFormError(t("usage.pricing.overrides.invalidRate"));
        return;
      }
      if (offpeakValues.every((value) => value.trim() === "")) {
        setFormError(t("usage.pricing.overrides.atLeastOneOffpeak"));
        return;
      }
      timeRule = {
        timezone,
        windows,
        offpeakInput: toRate(offpeakRates.input),
        offpeakCacheRead: toRate(offpeakRates.cacheRead),
        offpeakCacheWrite: toRate(offpeakRates.cacheWrite),
        offpeakOutput: toRate(offpeakRates.output),
      };
    }

    setBusy(true);
    try {
      await usagePricingOverrideSet({
        normalizedModelId,
        currency,
        input: toRate(rates.input),
        cacheRead: toRate(rates.cacheRead),
        cacheWrite: toRate(rates.cacheWrite),
        output: toRate(rates.output),
        timeRule,
      });
      // 统一失效链：priceOverrides 与 costs 都在 `["usage"]` 前缀下
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
      resetForm();
      setNotice(true);
    } catch (e) {
      // 失败保留草稿，便于修正后重试
      setActionError(e);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (normalizedModelId: string) => {
    setBusy(true);
    setActionError(null);
    try {
      await usagePricingOverrideDelete(normalizedModelId);
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
      setConfirmingId(null);
      setNotice(false);
    } catch (e) {
      setActionError(e);
    } finally {
      setBusy(false);
    }
  };

  const saveDisabled = busy || model.trim() === "";

  return (
    <section
      data-testid="price-override-manager"
      className="mt-3 space-y-2 border-t pt-3"
      aria-label={t("usage.pricing.overrides.title")}
    >
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="flex-1">
          <div className="flex items-center gap-1">
            <p className="text-sm font-medium">{t("usage.pricing.overrides.title")}</p>
            <UsageInfoTip
              label={t("usage.pricing.overrides.helpLabel")}
              testId="price-override-info"
            >
              {t("usage.pricing.overrides.help")}
            </UsageInfoTip>
          </div>
          <p className="text-muted-foreground text-xs">{t("usage.pricing.overrides.desc")}</p>
          <p className="text-muted-foreground text-[11px]">
            {t("usage.pricing.overrides.scopeAllChannels")}
          </p>
        </div>
        <Button
          size="sm"
          variant="outline"
          data-testid="price-override-add"
          disabled={busy}
          onClick={() => beginAdd()}
        >
          {t("usage.pricing.overrides.add")}
        </Button>
      </div>

      {notice ? (
        <p
          role="status"
          data-testid="price-override-saved"
          className="text-muted-foreground text-xs"
        >
          {t("usage.pricing.overrides.saved")}
        </p>
      ) : null}

      {actionError ? (
        <p role="alert" data-testid="price-override-error" className="text-destructive text-xs">
          {usageErrMsg(actionError, t)}
        </p>
      ) : null}

      {formOpen ? (
        <form
          data-testid="price-override-form"
          className="space-y-2 rounded-md border p-2"
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          <div className="grid gap-2 sm:grid-cols-2">
            <div className="space-y-1">
              <span className="flex items-center gap-1">
                <label htmlFor="price-override-model" className="text-muted-foreground text-xs">
                  {t("usage.pricing.overrides.model")}
                </label>
                <UsageInfoTip
                  label={t("usage.pricing.overrides.modelHelpLabel")}
                  testId="price-override-model-info"
                >
                  {t("usage.pricing.overrides.modelHint")}
                </UsageInfoTip>
              </span>
              <Input
                id="price-override-model"
                data-testid="price-override-model"
                className="h-8 font-mono text-xs"
                placeholder={t("usage.pricing.overrides.modelPlaceholder")}
                value={model}
                disabled={editingId !== null}
                onChange={(event) => {
                  setModel(event.currentTarget.value);
                  setFormError(null);
                }}
              />
            </div>
            <div className="space-y-1 sm:col-span-2">
              <span className="text-muted-foreground text-xs">
                {t("usage.pricing.overrides.currency")}
              </span>
              <div className="flex items-center gap-1.5">
                {(["USD", "CNY"] as const).map((key) => (
                  <Button
                    key={key}
                    type="button"
                    size="sm"
                    variant={currency === key ? "default" : "outline"}
                    aria-pressed={currency === key}
                    data-testid={`price-override-currency-${key.toLowerCase()}`}
                    onClick={() => setCurrency(key)}
                  >
                    {key}
                  </Button>
                ))}
              </div>
            </div>
            <div className="flex items-center justify-between gap-3 rounded-md border px-2 py-1.5 sm:col-span-2">
              <div className="min-w-0">
                <span className="flex items-center gap-1 text-xs font-medium">
                  {t("usage.pricing.overrides.timeEnabled")}
                  <UsageInfoTip
                    label={t("usage.pricing.overrides.timeHelpLabel")}
                    testId="price-override-time-info"
                  >
                    {t("usage.pricing.overrides.timeHint")}
                  </UsageInfoTip>
                </span>
                <p className="text-muted-foreground text-[11px]">
                  {t("usage.pricing.overrides.timeEnabledHint")}
                </p>
              </div>
              <Switch
                data-testid="price-override-time-toggle"
                aria-label={t("usage.pricing.overrides.timeEnabled")}
                checked={timeEnabled}
                onCheckedChange={(enabled) => {
                  setTimeEnabled(enabled);
                  setFormError(null);
                }}
              />
            </div>

            <div className="text-muted-foreground text-xs font-medium sm:col-span-2">
              {timeEnabled
                ? t("usage.pricing.overrides.peakRates")
                : t("usage.pricing.overrides.fixedRates")}
            </div>
            {RATE_FIELDS.map(({ key, labelKey, testId }) => (
              <label key={key} className="space-y-1">
                <span className="text-muted-foreground text-xs">
                  {t(labelKey)} ({currency})
                </span>
                <Input
                  data-testid={testId}
                  className="h-8 font-mono text-xs tabular-nums"
                  inputMode="decimal"
                  value={rates[key]}
                  onChange={(event) => setRate(key, event.currentTarget.value)}
                />
              </label>
            ))}

            {timeEnabled ? (
              <>
                <div className="space-y-2 rounded-md border p-2 sm:col-span-2">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <span className="text-xs font-medium">
                      {t("usage.pricing.overrides.peakWindows")}
                    </span>
                    <Button
                      type="button"
                      size="xs"
                      variant="outline"
                      data-testid="price-override-window-add"
                      onClick={() =>
                        setWindows((prev) => [...prev, { start: "09:00", end: "12:00" }])
                      }
                    >
                      {t("usage.pricing.overrides.addWindow")}
                    </Button>
                  </div>
                  <p className="text-muted-foreground text-[11px]">
                    {t("usage.pricing.overrides.weekdaysHint")}
                  </p>
                  {windows.map((window, index) => (
                    <div key={index} className="flex flex-wrap items-center gap-1.5">
                      <select
                        aria-label={t("usage.pricing.overrides.windowStart")}
                        data-testid={`price-override-window-start-${index}`}
                        className="bg-background h-8 rounded-md border px-2 text-xs"
                        value={window.start}
                        onChange={(event) => updateWindow(index, { start: event.target.value })}
                      >
                        {HOUR_OPTIONS.map((hour) => (
                          <option key={hour} value={hour}>
                            {hour}
                          </option>
                        ))}
                      </select>
                      <span className="text-muted-foreground text-xs">–</span>
                      <select
                        aria-label={t("usage.pricing.overrides.windowEnd")}
                        data-testid={`price-override-window-end-${index}`}
                        className="bg-background h-8 rounded-md border px-2 text-xs"
                        value={window.end}
                        onChange={(event) => updateWindow(index, { end: event.target.value })}
                      >
                        {HOUR_OPTIONS.map((hour) => (
                          <option key={hour} value={hour}>
                            {hour}
                          </option>
                        ))}
                      </select>
                      {window.start > window.end ? (
                        <span className="text-muted-foreground text-[11px]">
                          {t("usage.pricing.overrides.windowCrossMidnight")}
                        </span>
                      ) : null}
                      <Button
                        type="button"
                        size="xs"
                        variant="ghost"
                        data-testid={`price-override-window-remove-${index}`}
                        disabled={windows.length <= 1}
                        onClick={() =>
                          setWindows((prev) => prev.filter((_, item) => item !== index))
                        }
                      >
                        {t("usage.pricing.overrides.removeWindow")}
                      </Button>
                    </div>
                  ))}
                  <label className="flex flex-wrap items-center gap-2 text-xs">
                    <span className="text-muted-foreground">
                      {t("usage.pricing.overrides.timezone")}
                    </span>
                    <select
                      data-testid="price-override-timezone"
                      className="bg-background h-8 rounded-md border px-2 text-xs"
                      value={timezone}
                      onChange={(event) =>
                        setTimezone(event.currentTarget.value as TimePriceTimezone)
                      }
                    >
                      {TIMEZONE_OPTIONS.map((option) => (
                        <option key={option.value} value={option.value}>
                          {t(option.labelKey)}
                        </option>
                      ))}
                    </select>
                    <span className="text-muted-foreground text-[11px]">
                      {t("usage.pricing.overrides.timezoneHint")}
                    </span>
                  </label>
                </div>

                <div className="space-y-2 rounded-md border p-2 sm:col-span-2">
                  <div className="flex flex-wrap items-end gap-2">
                    <span className="text-xs font-medium">
                      {t("usage.pricing.overrides.offpeakRates")}
                    </span>
                    <label className="flex items-center gap-1 text-xs">
                      <span className="text-muted-foreground">
                        {t("usage.pricing.overrides.discountLabel")}
                      </span>
                      <Input
                        data-testid="price-override-discount"
                        className="h-8 w-16 font-mono text-xs"
                        inputMode="decimal"
                        value={discountPercent}
                        onChange={(event) => setDiscountPercent(event.currentTarget.value)}
                      />
                      <span className="text-muted-foreground">%</span>
                    </label>
                    <Button
                      type="button"
                      size="xs"
                      variant="outline"
                      data-testid="price-override-discount-apply"
                      onClick={applyDiscount}
                    >
                      {t("usage.pricing.overrides.applyDiscount")}
                    </Button>
                  </div>
                  <div className="grid gap-2 sm:grid-cols-2">
                    {RATE_FIELDS.map(({ key, labelKey }) => (
                      <label key={key} className="space-y-1">
                        <span className="text-muted-foreground text-xs">
                          {t(labelKey)} ({currency})
                        </span>
                        <Input
                          data-testid={`price-override-offpeak-${key}`}
                          className="h-8 font-mono text-xs tabular-nums"
                          inputMode="decimal"
                          value={offpeakRates[key]}
                          onChange={(event) => setOffpeakRate(key, event.currentTarget.value)}
                        />
                      </label>
                    ))}
                  </div>
                  <p className="text-muted-foreground text-[11px]">
                    {t("usage.pricing.overrides.offpeakHint")}
                  </p>
                </div>
              </>
            ) : null}
          </div>

          {formError ? (
            <p
              role="alert"
              data-testid="price-override-form-error"
              className="text-destructive text-xs"
            >
              {formError}
            </p>
          ) : null}

          <div className="flex items-center gap-2">
            <Button
              type="submit"
              size="sm"
              data-testid="price-override-save"
              disabled={saveDisabled}
            >
              {t("usage.pricing.overrides.save")}
            </Button>
            <Button
              type="button"
              size="sm"
              variant="outline"
              data-testid="price-override-cancel"
              disabled={busy}
              onClick={resetForm}
            >
              {t("usage.pricing.overrides.cancel")}
            </Button>
          </div>
        </form>
      ) : null}

      {overrides.isPending ? (
        <p className="text-muted-foreground text-xs">{t("common.loading")}</p>
      ) : overrides.isError ? (
        <div className="space-y-1" role="alert">
          <p className="text-destructive text-xs">{usageErrMsg(overrides.error, t)}</p>
          <Button size="sm" variant="outline" onClick={() => void overrides.refetch()}>
            {t("common.retry")}
          </Button>
        </div>
      ) : rows.length === 0 ? (
        <p className="text-muted-foreground text-xs">{t("usage.pricing.overrides.empty")}</p>
      ) : (
        <ul className="space-y-2">
          {rows.map((view) => {
            const id = view.normalizedModelId;
            const hasSource = view.sourceProvider !== null || view.sourceModel !== null;
            return (
              <li
                key={id}
                data-testid={`price-override-row-${id}`}
                className="space-y-1 rounded-md border p-2 text-xs"
              >
                <div className="flex flex-wrap items-start justify-between gap-2">
                  <div className="min-w-0 flex-1 space-y-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-mono font-medium">{id}</span>
                      <span
                        data-testid={`price-override-badge-${id}`}
                        className="bg-muted text-muted-foreground rounded px-1.5 py-0.5 text-[10px]"
                      >
                        {view.timeRule
                          ? t("usage.pricing.overrides.kindTime")
                          : t("usage.pricing.overrides.kindFixed")}
                      </span>
                      <span
                        data-testid={`price-override-currency-${id}`}
                        className="text-muted-foreground rounded border px-1.5 py-0.5 text-[10px]"
                      >
                        {view.currency}
                      </span>
                    </div>

                    <div className="text-muted-foreground flex flex-wrap gap-x-3 gap-y-1">
                      <span className="font-medium">
                        {view.timeRule
                          ? t("usage.pricing.overrides.peakRates")
                          : t("usage.pricing.overrides.fixedRates")}
                      </span>
                      {RATE_FIELDS.map(({ key, labelKey, testId }) => (
                        <span key={key} data-testid={`${testId}-${id}`}>
                          {t(labelKey)}: {view[key] ?? EM_DASH}
                        </span>
                      ))}
                    </div>

                    {view.timeRule ? (
                      <div
                        data-testid={`price-override-time-${id}`}
                        className="text-muted-foreground flex flex-wrap gap-x-3 gap-y-1"
                      >
                        <span data-testid={`price-override-time-schedule-${id}`}>
                          {t("usage.pricing.overrides.schedule")}:{" "}
                          {t("usage.pricing.overrides.weekdaysShort", {
                            windows: windowsLabel(view.timeRule.windows),
                            timezone: timezoneLabel(t, view.timeRule.timezone),
                          })}
                        </span>
                        <span data-testid={`price-override-offpeak-summary-${id}`}>
                          {t("usage.pricing.overrides.offpeakRates")}:{" "}
                          {RATE_FIELDS.map(({ key, labelKey }) => {
                            const value = offpeakRateOf(view.timeRule, key);
                            return `${t(labelKey)} ${value ?? EM_DASH}`;
                          }).join(" · ")}
                        </span>
                      </div>
                    ) : null}

                    <div className="text-muted-foreground flex flex-wrap gap-x-3 gap-y-1">
                      <span data-testid={`price-override-source-${id}`}>
                        {t("usage.pricing.overrides.source")}:{" "}
                        {hasSource
                          ? `${view.sourceProvider ?? EM_DASH} / ${view.sourceModel ?? EM_DASH}`
                          : t("usage.pricing.overrides.noCatalog")}
                      </span>
                      <span data-testid={`price-override-release-${id}`}>
                        {t("usage.pricing.overrides.releaseDate")}: {view.releaseDate ?? EM_DASH}
                      </span>
                      <span data-testid={`price-override-snapshot-${id}`}>
                        {t("usage.pricing.overrides.snapshotDate")}:{" "}
                        {view.snapshotDate === null ? EM_DASH : fmtDateTime(view.snapshotDate)}
                      </span>
                      <span data-testid={`price-override-updated-${id}`}>
                        {t("usage.cost.priceUpdated")}: {fmtDateTime(view.updatedAt)}
                      </span>
                    </div>
                  </div>

                  <div className="flex shrink-0 items-center gap-1">
                    <Button
                      size="xs"
                      variant="outline"
                      data-testid={`price-override-edit-${id}`}
                      disabled={busy}
                      onClick={() => beginEdit(view)}
                    >
                      {t("usage.pricing.overrides.edit")}
                    </Button>
                    <Button
                      size="xs"
                      variant="ghost"
                      data-testid={`price-override-delete-${id}`}
                      disabled={busy}
                      aria-expanded={confirmingId === id}
                      aria-controls={
                        confirmingId === id ? `price-override-confirm-${id}` : undefined
                      }
                      onClick={() => setConfirmingId(id)}
                    >
                      {t("usage.pricing.overrides.delete")}
                    </Button>
                  </div>
                </div>

                {confirmingId === id ? (
                  <div
                    id={`price-override-confirm-${id}`}
                    data-testid={`price-override-confirm-${id}`}
                    className="flex flex-wrap items-center gap-2 border-t pt-1"
                    role="alert"
                    aria-live="polite"
                  >
                    <span>{t("usage.pricing.overrides.deleteConfirm")}</span>
                    <Button
                      ref={confirmRef}
                      size="xs"
                      variant="destructive"
                      data-testid={`price-override-delete-confirm-${id}`}
                      disabled={busy}
                      onClick={() => void remove(id)}
                    >
                      {t("common.confirm")}
                    </Button>
                    <Button
                      size="xs"
                      variant="outline"
                      data-testid={`price-override-delete-cancel-${id}`}
                      disabled={busy}
                      onClick={() => setConfirmingId(null)}
                    >
                      {t("common.cancel")}
                    </Button>
                  </div>
                ) : null}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

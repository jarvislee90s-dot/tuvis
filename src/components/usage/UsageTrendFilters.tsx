// B 趋势局部筛选（V2 Task 9）——原生 select + 现有 Button，紧凑一行。
//
// 单次只激活一个维度：全部 / 按工具 / 按渠道。渠道选项全部来自后端
// `ChannelOption`（显示渠道：显式绑定 / 别名 / 原始 provider；后端已排除
// unassigned/ambiguous 与归档渠道）。**不再有每工具「未指定渠道」选项**。
//  - 目标值 = `channel.key`（`bp:<profileId>` / `provider:<displayName>`），
//    写进 `TrendSelection.billingProfile.selectedId`；
//  - 选中项不在当前选项中（改名/归档/切范围）→ 显示原因并允许一键回到「全部」；
//  - 不把状态写回全局，A/C 不受影响。
import { useEffect } from "react";
import { Button } from "@/components/ui/button";
import { usageAgentLabel } from "@/lib/usage/format";
import type { TFn } from "@/lib/usage/range";
import { FILTER_TOOL_IDS } from "@/lib/usage/useRecordsFilters";
import type { ChannelOption, TrendSelection } from "@/types/usageV2";

type TrendMode = "all" | "tool" | "billingProfile";

export interface UsageTrendFiltersProps {
  selection: TrendSelection;
  onChange(selection: TrendSelection): void;
  /** 后端渠道选项（只含可筛选渠道；来源由页面查询提供） */
  channels: readonly ChannelOption[];
  /** 渠道选项查询仍在加载：显示加载态，不显示「所选对象不可得」 */
  channelsPending?: boolean;
  /** 渠道选项查询失败原因（已本地化）；与重试按钮一起就地展示 */
  channelsError?: string | null;
  onRetryChannels?(): void;
  /** 后端返回 `UsageTrend.available=false`（当前选择不可得） */
  unavailable?: boolean;
  t: TFn;
}

function modeOf(selection: TrendSelection): TrendMode {
  if (selection.scope === "all") return "all";
  if (selection.scope === "tool") return "tool";
  return "billingProfile";
}

function targetValueOf(selection: TrendSelection): string {
  if (selection.scope === "tool") return `tool:${selection.selectedId}`;
  if (selection.scope === "billingProfile") return selection.selectedId;
  if (selection.scope === "unknown") return `unknown:${selection.selectedId}`;
  return "";
}

/** 切到「按渠道」时优先第一个 ChannelOption；选项还没到时先留空，待选项到达后再补选。 */
function firstTargetOf(mode: TrendMode, channels: readonly ChannelOption[]): TrendSelection {
  if (mode === "tool") return { scope: "tool", selectedId: FILTER_TOOL_IDS[0] };
  if (mode === "billingProfile")
    return { scope: "billingProfile", selectedId: channels[0]?.key ?? "" };
  return { scope: "all" };
}

const selectClass =
  "h-8 max-w-[11rem] rounded-md border bg-background px-2 text-xs whitespace-nowrap";

/**
 * 失效选中键的可读占位名：
 *  * `provider:<显示名>` 直接取后缀（别名/原始 provider 均已是可读名）；
 *  * `bp:<profileId>` 无 profile 字典可用 → 统一显示「已失效渠道」，不把裸键印给用户。
 */
function channelPlaceholderLabel(key: string, t: TFn): string {
  if (key.startsWith("provider:")) {
    const name = key.slice("provider:".length).trim();
    if (name !== "") return name;
  }
  return t("usage.trend.invalidChannel");
}

export function UsageTrendFilters({
  selection,
  onChange,
  channels,
  channelsPending = false,
  channelsError = null,
  onRetryChannels,
  unavailable = false,
  t,
}: UsageTrendFiltersProps) {
  const mode = modeOf(selection);
  const targetValue = targetValueOf(selection);

  // 首次切到渠道模式时选项可能仍在加载：只补选空键，不改写已失效的选择
  // （失效选择必须保留并显示原因，让用户显式回到「全部」）。
  useEffect(() => {
    if (
      selection.scope === "billingProfile" &&
      selection.selectedId === "" &&
      channels.length > 0
    ) {
      onChange({ scope: "billingProfile", selectedId: channels[0].key });
    }
  }, [channels, onChange, selection]);

  const knownTarget =
    mode === "tool"
      ? selection.scope === "tool" && FILTER_TOOL_IDS.includes(selection.selectedId)
      : mode === "billingProfile"
        ? selection.scope === "billingProfile" &&
          (selection.selectedId === "" ||
            channels.some((channel) => channel.key === selection.selectedId))
        : true;
  // 已失效的选中键仍作为占位选项显示（否则原生 select 会显示空白，用户不知道原选择）。
  const missingChannel =
    mode === "billingProfile" &&
    selection.scope === "billingProfile" &&
    selection.selectedId !== "" &&
    !channels.some((channel) => channel.key === selection.selectedId)
      ? selection.selectedId
      : null;
  // 渠道选项三态：加载中 / 失败 / 就绪。加载与失败都不显示「所选对象不可得」。
  const channelState =
    mode === "billingProfile"
      ? channelsPending
        ? "pending"
        : channelsError
          ? "error"
          : "ready"
      : "ready";
  const showUnavailable = channelState === "ready" && (!knownTarget || unavailable);

  const handleMode = (next: TrendMode) => {
    // 保留当前维度内已有的具体对象；跨维度时取该维度的第一个可用对象。
    if (next === "all") return onChange({ scope: "all" });
    if (next === mode && selection.scope !== "all") return;
    onChange(firstTargetOf(next, channels));
  };

  const handleTarget = (value: string) => {
    if (mode === "tool") {
      return onChange({
        scope: "tool",
        selectedId: value.replace(/^tool:/, "") as (typeof FILTER_TOOL_IDS)[number],
      });
    }
    if (mode === "billingProfile") return onChange({ scope: "billingProfile", selectedId: value });
  };

  return (
    <div className="flex flex-wrap items-center justify-end gap-1.5">
      <label className="text-muted-foreground flex items-center gap-1 text-xs">
        <span>{t("usage.trend.mode")}</span>
        <select
          data-testid="usage-trend-mode"
          className={selectClass}
          value={mode}
          onChange={(e) => handleMode(e.target.value as TrendMode)}
        >
          <option value="all">{t("usage.trend.all")}</option>
          <option value="tool">{t("usage.trend.byTool")}</option>
          <option value="billingProfile">{t("usage.trend.byChannel")}</option>
        </select>
      </label>

      {mode !== "all" ? (
        <label className="text-muted-foreground flex items-center gap-1 text-xs">
          <span>{t("usage.trend.target")}</span>
          <select
            data-testid="usage-trend-target"
            className={selectClass}
            value={targetValue}
            onChange={(e) => handleTarget(e.target.value)}
          >
            {mode === "tool"
              ? FILTER_TOOL_IDS.map((id) => (
                  <option key={id} value={`tool:${id}`}>
                    {usageAgentLabel(id)}
                  </option>
                ))
              : [
                  ...(mode === "billingProfile" && channels.length === 0
                    ? [
                        <option key="__channels-empty" value="">
                          {channelsPending ? t("common.loading") : t("usage.trend.noChannels")}
                        </option>,
                      ]
                    : []),
                  ...(missingChannel
                    ? [
                        <option
                          key={missingChannel}
                          value={missingChannel}
                          data-testid="usage-trend-target-missing"
                        >
                          {channelPlaceholderLabel(missingChannel, t)}
                        </option>,
                      ]
                    : []),
                  ...channels.map((channel) => (
                    <option key={channel.key} value={channel.key}>
                      {channel.label}
                    </option>
                  )),
                ]}
          </select>
        </label>
      ) : null}

      {channelState === "pending" ? (
        <span data-testid="usage-trend-channels-loading" className="text-muted-foreground text-xs">
          {t("common.loading")}
        </span>
      ) : null}

      {channelState === "error" ? (
        <>
          <span data-testid="usage-trend-channels-error" className="text-destructive text-xs">
            {channelsError}
          </span>
          {onRetryChannels ? (
            <Button
              size="sm"
              variant="outline"
              data-testid="usage-trend-channels-retry"
              onClick={onRetryChannels}
            >
              {t("common.retry")}
            </Button>
          ) : null}
        </>
      ) : null}

      {showUnavailable ? (
        <>
          <span
            data-testid="usage-trend-filter-unavailable"
            className="text-muted-foreground text-xs"
          >
            {t("usage.trend.filterUnavailable")}
          </span>
          <Button
            size="sm"
            variant="outline"
            data-testid="usage-trend-reset"
            onClick={() => onChange({ scope: "all" })}
          >
            {t("usage.trend.reset")}
          </Button>
        </>
      ) : null}
    </div>
  );
}

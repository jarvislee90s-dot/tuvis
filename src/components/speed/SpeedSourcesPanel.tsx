// 二级采集管理面板（2026-10-26 计划 Task 11）——速度页与设置页共用的同一套接入流程。
//
// 纪律（Spec §8.6/§8.7）：
//  * 来源一级**只**显示名称、接入状态、指标能力与动作；已接入/待配置/待重启/待检查按真实
//    状态显示；Codex =「已接入 · 仅响应」——**不**把它整体标为不支持，流缺口只归入流能力
//    的短原因（经 `translateSpeedReason`，机器 token 不上一级行）；
//  * 已安装事实决定卸载/重装入口：`installed=false` 给接入（即重装），卸载后保留历史；
//  * 接入引导 = 预览（写哪个原生设置文件 + 脱敏对照）→ 应用 → 下次启动生效；Tuvis 不主动
//    重启 Harness（动作流经共享 `TpsInstallFlow`，不复制安装算法）；
//  * 版本/路径/字段与脱敏诊断（`detail`）**只在展开的 details 区**显示；Mac 界面一级不堆
//    Windows 缺口长文；
//  * `lastRecordAt` 是中性提示：无记录 = 「—」，无区间记录是数据空态，**不**按采集故障渲染；
//  * 本面板自身**不做任何轮询、不申请视图租约**：`sources` 由宿主注入（速度页 = 自己的
//    dashboard 查询；设置页 = open 且窗口可见时的单次取数），动作成功后的 `["tps"]` 前缀
//    失效内聚在 TpsInstallFlow。
import { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { TpsInstallFlow, TPS_INSTALLABLE } from "@/components/usage/tps/TpsInstallFlow";
import { TPS_KEY } from "@/lib/query/queries/tps";
import { translateSpeedReason } from "@/lib/speed/view";
import { EM_DASH, usageAgentLabel } from "@/lib/usage/format";
import type { SpeedSourceView } from "@/types/speed";

/** 单指标能力小徽章：可用素色呈现；缺口 = 灰 chip + 短原因（translateSpeedReason 出口）。 */
function CapChip({
  testId,
  label,
  ok,
  reason,
}: {
  testId: string;
  label: string;
  ok: boolean;
  reason: string | null;
}) {
  const { t } = useAppTranslation();
  return (
    <span
      data-testid={testId}
      className={
        ok
          ? "rounded bg-emerald-500/10 px-1.5 py-0.5 text-[10px] text-emerald-700 dark:text-emerald-400"
          : "bg-muted text-muted-foreground rounded px-1.5 py-0.5 text-[10px]"
      }
    >
      {label}
      {ok ? "" : ` · ${reason ?? t("speed.sources.capUnavailable")}`}
    </span>
  );
}

/** 单来源一级行 + 按需展开的技术详情区。 */
function SourceRow({
  source,
  busy,
  startInstall,
  uninstall,
  receiptTool,
  expanded,
  onToggleDetails,
}: {
  source: SpeedSourceView;
  busy: boolean;
  startInstall: (tool: string) => void;
  uninstall: (tool: string) => void;
  receiptTool: string | null;
  expanded: boolean;
  onToggleDetails: () => void;
}) {
  const { t } = useAppTranslation();
  const { capabilities } = source;
  const installable = TPS_INSTALLABLE.has(source.source);
  // Codex（Ready + 仅响应）：整体状态不降级，缺口只以「仅响应」+ 流短原因呈现（Spec §8.6）
  const responseOnly = source.status === "ready" && !capabilities.stream && capabilities.response;
  // 卸载入口跟随安装事实（Spec §8.6「已安装事实决定卸载入口」）：无安装机制的来源
  // （后端恒 installed=true）不给无意义的卸载；刚应用成功的回执作为安装事实回流前的过渡。
  const canUninstall = (source.installed && installable) || receiptTool === source.source;
  // 接入（=重装）入口：未安装即可接入——卸载后保留的历史不得挡住重装入口。
  const canInstall = installable && (!source.installed || source.status === "unconfigured");
  const reasonText = source.reason ? translateSpeedReason(t, source.reason) : null;
  return (
    <div className="rounded-md border p-2" data-testid={`speed-source-${source.source}`}>
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
        <span className="font-medium">{usageAgentLabel(source.source)}</span>
        <span
          data-testid={`speed-status-${source.source}`}
          className={
            source.status === "ready" ? "text-xs" : "text-xs text-amber-600 dark:text-amber-500"
          }
        >
          {t(`speed.status.${source.status}`)}
          {responseOnly ? ` · ${t("speed.sources.responseOnly")}` : ""}
        </span>
        <CapChip
          testId={`speed-cap-stream-${source.source}`}
          label={t("speed.sources.capStream")}
          ok={capabilities.stream}
          reason={
            capabilities.streamReason ? translateSpeedReason(t, capabilities.streamReason) : null
          }
        />
        <CapChip
          testId={`speed-cap-response-${source.source}`}
          label={t("speed.sources.capResponse")}
          ok={capabilities.response}
          reason={
            capabilities.responseReason
              ? translateSpeedReason(t, capabilities.responseReason)
              : null
          }
        />
        {/* 状态成因的短原因（机器 token 已翻译；raw detail 只进 details 区） */}
        {reasonText ? <span className="text-muted-foreground">{reasonText}</span> : null}
        <span className="flex-1" />
        {canInstall ? (
          <Button
            size="sm"
            variant="outline"
            disabled={busy}
            data-testid={`speed-install-${source.source}`}
            onClick={() => startInstall(source.source)}
          >
            {t("speed.sources.actionInstall")}
          </Button>
        ) : null}
        {canUninstall ? (
          <Button
            size="sm"
            variant="ghost"
            disabled={busy}
            data-testid={`speed-uninstall-${source.source}`}
            onClick={() => uninstall(source.source)}
          >
            {t("speed.sources.actionUninstall")}
          </Button>
        ) : null}
        <Button
          size="sm"
          variant="ghost"
          aria-expanded={expanded}
          data-testid={`speed-details-toggle-${source.source}`}
          onClick={onToggleDetails}
        >
          {t("speed.sources.actionDetails")}
        </Button>
      </div>
      {/* 最近记录：中性提示（无记录 = —），数据空态不是采集故障 */}
      <div
        className="text-muted-foreground mt-1 text-[11px]"
        data-testid={`speed-last-record-${source.source}`}
      >
        {t("speed.sources.lastRecord")}：
        {source.lastRecordAt === null ? EM_DASH : new Date(source.lastRecordAt).toLocaleString()}
      </div>
      {expanded ? (
        <div
          className="bg-muted/30 mt-2 space-y-1 rounded-md border p-2"
          data-testid={`speed-details-${source.source}`}
        >
          <p className="text-muted-foreground text-[11px] font-medium">
            {t("speed.sources.detailsTitle")}
          </p>
          {reasonText ? (
            <p className="text-[11px]">
              {t("speed.sources.reasonLabel")}：{reasonText}
            </p>
          ) : null}
          {capabilities.streamReason ? (
            <p className="text-muted-foreground text-[11px]">
              {t("speed.sources.capStream")}：{translateSpeedReason(t, capabilities.streamReason)}
            </p>
          ) : null}
          {capabilities.responseReason ? (
            <p className="text-muted-foreground text-[11px]">
              {t("speed.sources.capResponse")}：
              {translateSpeedReason(t, capabilities.responseReason)}
            </p>
          ) : null}
          {source.detail ? (
            // 既有诊断原文（含 `parser-errors=` 等机器前缀）：只在此处原样透出，不翻译不加工
            <code className="text-muted-foreground block text-[11px] break-all whitespace-pre-wrap">
              {source.detail}
            </code>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

/**
 * 采集管理面板（受控弹层）。`sources` 来自宿主的 `SpeedDashboardSnapshot.sourceStates`
 * （速度页 = 自身 dashboard 查询；设置页 = open 且窗口可见时的单次取数）。
 * 面板自身零轮询、零租约；「刷新」缺省只失效 `["tps"]` 前缀（含 `["tps","v2"]`），
 * 由宿主查询按各自的 active 门控决定是否立即重取。
 *
 * 取数三态由宿主经**可选**的 `fetchState` 注入（缺省 "ready"，保持绑定三参接口不变）：
 * 错误 = 内联错误 + 「刷新」作手动重试（`onRetry`，仍单次语义，错误后绝不自动轮询）；
 * 首次加载 = 加载占位；真·无来源 = 中性空态——三者 UI 互不冒充
 * （Spec §8 尾「无区间记录是数据空态，不自动等于采集故障」的反面同样成立：
 * 取数故障不得伪装成空态）。
 */
export function SpeedSourcesPanel({
  open,
  onOpenChange,
  sources,
  fetchState = "ready",
  onRetry,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  sources: SpeedSourceView[];
  /** 宿主取数状态：error 渲染内联错误；loading（首次无数据）渲染占位；缺省 ready */
  fetchState?: "loading" | "error" | "ready";
  /** 错误/刷新时宿主的重试动作（设置侧 = 重武装单次取数状态机）；缺省退回 ["tps"] 失效 */
  onRetry?: () => void;
}) {
  const { t } = useAppTranslation();
  const queryClient = useQueryClient();
  const [detailsFor, setDetailsFor] = useState<string | null>(null);
  // 关闭即收起详情：面板不留自己的会话残留（数据 pin 也没有——数据永远来自宿主 props）
  useEffect(() => {
    if (!open) setDetailsFor(null);
  }, [open]);
  const refreshFacts = () => {
    if (onRetry) {
      onRetry();
      return;
    }
    void queryClient.invalidateQueries({ queryKey: TPS_KEY });
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="workspace-glass max-h-[85vh] overflow-y-auto sm:max-w-lg"
        data-testid="speed-sources-panel"
      >
        <DialogHeader>
          <div className="flex items-start justify-between gap-2">
            <div className="min-w-0">
              <DialogTitle>{t("speed.sources.title")}</DialogTitle>
              <DialogDescription>{t("speed.sources.desc")}</DialogDescription>
            </div>
            <Button
              size="sm"
              variant="ghost"
              data-testid="speed-sources-refresh"
              onClick={() => refreshFacts()}
            >
              <RefreshCw className="mr-1 h-3 w-3" />
              {t("speed.sources.refresh")}
            </Button>
          </div>
        </DialogHeader>
        {fetchState === "error" ? (
          // 取数故障≠数据空态：内联可见可重试（样式沿 UsageSection 的错误行惯例），错误后不自动轮询
          <p role="alert" className="text-sm text-red-500" data-testid="speed-sources-error">
            {t("speed.sources.fetchError")}
          </p>
        ) : null}
        <TpsInstallFlow t={t}>
          {({ busy, startInstall, uninstall, receiptTool }) => (
            <div className="space-y-1" data-testid="speed-sources-list">
              {fetchState === "error" && sources.length === 0 ? null : sources.length === 0 ? (
                fetchState === "loading" ? (
                  // 首次加载占位：与「真·无来源」空态在 UI 上区分，不闪空态文案
                  <p className="text-muted-foreground text-xs" data-testid="speed-sources-loading">
                    {t("common.loading")}
                  </p>
                ) : (
                  <p className="text-muted-foreground text-xs">{t("speed.sources.empty")}</p>
                )
              ) : (
                sources.map((s) => (
                  <SourceRow
                    key={s.source}
                    source={s}
                    busy={busy}
                    startInstall={startInstall}
                    uninstall={uninstall}
                    receiptTool={receiptTool}
                    expanded={detailsFor === s.source}
                    onToggleDetails={() =>
                      setDetailsFor((cur) => (cur === s.source ? null : s.source))
                    }
                  />
                ))
              )}
            </div>
          )}
        </TpsInstallFlow>
      </DialogContent>
    </Dialog>
  );
}

// 一次配置安装流的**共享承载**（2026-10-26 计划 Task 11，自 TpsSourceStates 原样抽出）：
// 预览 → 原生信任说明（写哪个原生设置文件 + 脱敏字段对照）→ 应用 → 回执（下次启动生效）。
// 消费者 = TpsSourceStates（旧 TPS 卡）与 SpeedSourcesPanel（V2 采集管理面板）——
// 预览/应用/卸载动作与脱敏渲染**不复制第二份**（brief：复用配置操作包装，不复制安装算法）。
//
// 纪律（沿承原实现，Spec §1/§2/§4/§5）：
//  * 安装引导 = **一次点击**（preview → apply），提示「下次启动生效」；**绝不**替用户
//    重启/终止 Harness；
//  * 脱敏标记归一：回执 `[已脱敏]` 与预览 `[redacted <指纹>]` 在展示层同权渲染为
//    脱敏 chip，不回显原文标记；
//  * 安装族错误用 reason 字段定位（conflict / stale-preview / …），默认 append-only，
//    冲突时才需要用户显式「转交统计目标」；
//  * 动作成功后失效 `["tps"]` 查询前缀（含 V2 的 `["tps","v2"]`）——安装事实是持久态，
//    宿主入口立即按新事实重算，不等轮询（独立复核 R10'）。
import { useState, type ReactElement, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { EM_DASH } from "@/lib/usage/format";
import { isMaskedMarker } from "@/lib/usage/tpsSeries";
import { tpsInstallApply, tpsInstallPreview, tpsUninstall } from "@/lib/api/tps";
import { TPS_KEY } from "@/lib/query/queries/tps";
import type { TFn } from "@/lib/usage/range";
import type { TpsInstallChoice, TpsInstallPreview, TpsInstallReceipt } from "@/types/tps";

/** 一次配置流适用的来源（原生只读来源不装统计键）。V1/V2 共用同一份安装机制名单。 */
export const TPS_INSTALLABLE = new Set(["claude", "opencode"]);

/** reason → i18n 键段（与 commands/tps.rs 的 install_error_reason 映射表对齐）。 */
function reasonKey(reason: string): string | null {
  const TABLE: Record<string, string> = {
    conflict: "conflict",
    "stale-preview": "stale-preview",
    "expired-preview": "expired-preview",
    "preview-not-found": "preview-not-found",
    "uninstall-conflict": "uninstall-conflict",
  };
  return TABLE[reason] ?? null;
}

/** 值显示：脱敏标记 → chip；否则原样进 code（非敏感键本来就是明文统计项）。 */
function maskedOrRaw(v: string, t: TFn): ReactElement {
  return isMaskedMarker(v) ? (
    <span
      data-testid="tps-masked-chip"
      className="bg-muted text-muted-foreground rounded px-1.5 py-0.5 text-[11px]"
    >
      {t("usage.tps.sources.masked")}
    </span>
  ) : (
    <code className="text-xs">{v}</code>
  );
}

/** 渲染槽收到的动作接口：面板用它把动作挂到自己的行按钮上。 */
export interface TpsInstallFlowApi {
  /** 任一动作在途（预览/应用/卸载）：宿主按钮据此禁用 */
  busy: boolean;
  /** 发起一次安装预览（tool） */
  startInstall: (tool: string) => void;
  /** 卸载该来源的自有统计项 */
  uninstall: (tool: string) => void;
  /** 最近一次应用成功的回执所属 tool（宿主可在安装事实尚未回流时先给卸载入口） */
  receiptTool: string | null;
}

/**
 * 一次配置安装流（预览 → 应用 → 回执 + 内联错误提示）。
 * `children` 是渲染槽：宿主渲染自己的来源行，动作经 api 挂到行按钮；
 * 预览块 / 回执块 / 内联提示（role=status）由本组件统一渲染在槽之后。
 * 动作成功后失效 `["tps"]` 前缀（含 `["tps","v2"]`）——与原 TpsSourceStates 行为逐字一致。
 */
export function TpsInstallFlow({
  t,
  children,
}: {
  t: TFn;
  children: (api: TpsInstallFlowApi) => ReactNode;
}) {
  const [preview, setPreview] = useState<TpsInstallPreview | null>(null);
  const [receipt, setReceipt] = useState<TpsInstallReceipt | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const queryClient = useQueryClient();
  // 配置动作改变的是**持久安装事实**（tps_state 安装记录）——动作成功后立即失效 TPS 查询，
  // 让入口按新事实重算，不等 5 秒轮询（独立复核 R10'）。["tps"] 前缀含 ["tps","v2"]。
  const refreshFacts = () => queryClient.invalidateQueries({ queryKey: TPS_KEY });

  async function run(fn: () => Promise<void>): Promise<void> {
    setBusy(true);
    setNote(null);
    try {
      await fn();
    } catch (e) {
      const reason = (e as { reason?: unknown } | null)?.reason;
      const key = typeof reason === "string" ? reasonKey(reason) : null;
      setNote(key ? t(`usage.tps.sources.reason.${key}`) : usageErrMsg(e, t));
    } finally {
      setBusy(false);
    }
  }

  const doPreview = (tool: string) =>
    void run(async () => {
      setReceipt(null);
      setPreview(await tpsInstallPreview(tool));
    });
  const doApply = (choice: TpsInstallChoice) =>
    void run(async () => {
      if (!preview) return;
      setReceipt(await tpsInstallApply(preview.previewId, choice));
      setPreview(null);
      await refreshFacts();
    });
  const doUninstall = (tool: string) =>
    void run(async () => {
      await tpsUninstall(tool);
      setReceipt(null);
      setNote(t("usage.tps.sources.uninstalledNote"));
      await refreshFacts();
    });

  return (
    <>
      {children({
        busy,
        startInstall: doPreview,
        uninstall: doUninstall,
        receiptTool: receipt?.tool ?? null,
      })}

      {note ? (
        <p role="status" className="text-muted-foreground text-xs">
          {note}
        </p>
      ) : null}

      {preview ? (
        <div className="space-y-1.5 rounded-md border p-2" data-testid="tps-preview">
          <p className="text-xs font-medium">{t("usage.tps.sources.previewTitle")}</p>
          <p className="text-muted-foreground truncate text-[11px]">{preview.settingsPath}</p>
          {preview.entries.map((e) => (
            <div key={e.key} className="flex flex-wrap items-center gap-2 text-xs">
              <code>{e.key}</code>
              <span className="text-muted-foreground">{t("usage.tps.sources.oldValue")}:</span>
              {e.oldMasked === null ? EM_DASH : maskedOrRaw(e.oldMasked, t)}
              <span className="text-muted-foreground">{t("usage.tps.sources.planned")}:</span>
              {maskedOrRaw(e.plannedMasked, t)}
              {e.conflict ? <span>{t("usage.tps.sources.conflictFlag")}</span> : null}
            </div>
          ))}
          <div className="flex flex-wrap items-center gap-1.5">
            <Button
              size="sm"
              disabled={busy || preview.conflictKeys.length > 0}
              data-testid="tps-apply"
              onClick={() => void doApply("append-only")}
            >
              {t("usage.tps.sources.choiceAppend")}
            </Button>
            {preview.conflictKeys.length > 0 ? (
              <Button
                size="sm"
                variant="outline"
                disabled={busy}
                data-testid="tps-apply-transfer"
                onClick={() => void doApply("transfer-statistics-target")}
              >
                {t("usage.tps.sources.choiceTransfer")}
              </Button>
            ) : null}
            <Button size="sm" variant="ghost" disabled={busy} onClick={() => setPreview(null)}>
              {t("common.cancel")}
            </Button>
          </div>
        </div>
      ) : null}

      {receipt ? (
        <div className="space-y-1 rounded-md border p-2" data-testid="tps-receipt">
          {receipt.entries.map((e) => (
            <div key={e.key} className="flex flex-wrap items-center gap-2 text-xs">
              <code>{e.key}</code>
              {maskedOrRaw(e.installedValue, t)}
            </div>
          ))}
          {receipt.receiver ? (
            <p className="text-muted-foreground text-[11px]">
              {t("usage.tps.sources.receiverPort", { port: receipt.receiver.port })}
            </p>
          ) : null}
          <p className="text-muted-foreground text-xs" data-testid="tps-restart-hint">
            {t("usage.tps.sources.restartHint")}
          </p>
        </div>
      ) : null}
    </>
  );
}

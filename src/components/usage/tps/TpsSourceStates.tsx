// TPS 来源状态列表（Task 8；2026-10-26 计划 Task 11 拆出动作流）。
//
// 纪律（Spec §1/§2/§4/§5）：
//  * 来源状态**只认** detail 的 `tps-status=` 前缀（不认 ok 布尔——合成 unconfigured 态
//    当前 ok=true）；Codex 缺口单列「本版不提供TPS」，不给假 TPS；
//  * 安装引导 = **一次点击**（preview → apply），提示「下次启动生效」；**绝不**替用户
//    重启/终止 Harness——预览/应用/卸载与脱敏渲染经共享的 `TpsInstallFlow`（本组件
//    只保留行渲染与入口判据，不复制安装算法）；
//  * 安装/卸载入口只认**持久安装事实**（detail 的 `tps-installed=` token，独立复核 R10'），
//    不依赖本组件临时状态；仅「刚应用成功的回执」作为安装事实回流前的过渡入口保留。
import { Button } from "@/components/ui/button";
import { TpsInstallFlow, TPS_INSTALLABLE } from "@/components/usage/tps/TpsInstallFlow";
import { parseTpsStatus } from "@/lib/usage/tpsSeries";
import type { TFn } from "@/lib/usage/range";
import { usageAgentLabel } from "@/lib/usage/format";
import type { TpsSourceState } from "@/types/tps";

export function TpsSourceStates({ states, t }: { states: TpsSourceState[]; t: TFn }) {
  return (
    <div className="space-y-2" data-testid="tps-sources">
      <p className="text-muted-foreground text-xs font-medium">{t("usage.tps.sources.title")}</p>
      <TpsInstallFlow t={t}>
        {({ busy, startInstall, uninstall, receiptTool }) => (
          <div className="space-y-1">
            {states.map((st) => {
              const parsed = parseTpsStatus(st.detail);
              return (
                <div
                  key={st.source}
                  data-testid={`tps-source-${st.source}`}
                  className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs"
                >
                  <span className="font-medium">{usageAgentLabel(st.source)}</span>
                  <span
                    className={
                      parsed.status === "active"
                        ? "text-xs"
                        : "text-xs text-amber-600 dark:text-amber-500"
                    }
                  >
                    {t(`usage.tps.sources.status.${parsed.status ?? "unknown"}`)}
                  </span>
                  {/* Codex 缺口单列（Spec §4）：如实说明，不提供 TPS */}
                  {parsed.status === "insufficient-timing" ? (
                    <span className="text-muted-foreground">{t("usage.tps.sources.codexGap")}</span>
                  ) : null}
                  {parsed.rest ? (
                    <span className="text-muted-foreground/70 truncate">{parsed.rest}</span>
                  ) : null}
                  {/* 安装入口只认持久安装事实（独立复核 R10'）：未安装即可安装——卸载后保留的
                      24h 历史会让状态停在 active/no-sample/read-failed，不得因此没入口；无 token
                      的旧后端才回退按 unconfigured 判 */}
                  {TPS_INSTALLABLE.has(st.source) &&
                  (parsed.installed === false ||
                    (parsed.installed === null && parsed.status === "unconfigured")) ? (
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={busy}
                      data-testid={`tps-install-${st.source}`}
                      onClick={() => startInstall(st.source)}
                    >
                      {t("usage.tps.sources.install")}
                    </Button>
                  ) : null}
                  {parsed.status === "await-restart" ? (
                    <span className="text-muted-foreground">
                      {t("usage.tps.sources.restartHint")}
                    </span>
                  ) : null}
                  {parsed.installed === true ||
                  receiptTool === st.source ||
                  parsed.status === "await-restart" ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={busy}
                      data-testid={`tps-uninstall-${st.source}`}
                      onClick={() => uninstall(st.source)}
                    >
                      {t("usage.tps.sources.uninstall")}
                    </Button>
                  ) : null}
                </div>
              );
            })}
          </div>
        )}
      </TpsInstallFlow>
    </div>
  );
}

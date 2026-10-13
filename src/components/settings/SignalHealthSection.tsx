// 设置页「信号健康度」分区（T5：hook 通道自查面板）。
// 待办判据 = 已注册 ∧ 该工具有活跃会话 ∧ 零事件 → 提示在工具终端输入 /hooks 信任
// 兔维斯 条目（codex 信任门：注册成功 ≠ 事件触发，TUI 内人工信任一次后 hash 落用户层）。
// 数据源 = hook_signal_health（后端复用会话扫描快照 + 30s TTL 事件目录既有信息，
// 零新增扫描预算）；挂载/手动刷新各拉一次，无轮询（同 AuditLogSection 惯例）。
import { SETTINGS_FIELD } from "@/components/settings/typography";
import { SETTINGS_CARD_TITLE } from "@/components/settings/typography";
import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import { toast } from "sonner";
import { ToolIcon } from "@/components/common/ToolIcon";
import { useAppTranslation } from "@/hooks/use-app-translation";

// Rust ToolSignalHealth（monitor/hooks.rs，serde camelCase）前端同构
type ToolSignalHealth = {
  toolId: string;
  label: string;
  registered: boolean;
  hasActiveSessions: boolean;
  lastEventAt: string | null;
  // 1A 未注册原因（值域见后端 ToolSignalHealth 注释；本批仅 "helperNotShipped"）。
  // 可选防御：mock/旧后端不带该字段，按 undefined = 普通未注册处理
  unregisteredReason?: string | null;
};

export function SignalHealthSection() {
  const { t } = useAppTranslation();
  const [items, setItems] = useState<ToolSignalHealth[]>([]);
  // 加载失败可见（用户主动动作的失败不得伪装成「空态」，同 AuditLogSection 口径）
  const [loadError, setLoadError] = useState(false);
  // 刷新在途标记：仅用于禁用刷新按钮（读操作幂等，连点无新语义）
  const [loading, setLoading] = useState(false);
  // 快照时钟（refresh 成功时置位；相对时间渲染基准，见 ago 注释）
  const [now, setNow] = useState<number | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      // 兜底空表：mock/旧后端异常载荷不致 items.map 崩溃（同 settings.tsx `?? []` 防御）
      const payload = (await invoke<ToolSignalHealth[]>("hook_signal_health")) ?? [];
      setItems(payload);
      // 快照时钟：相对时间以数据拉取时刻为基准（事件本是拉取那一刻的 30s TTL
      // 窗口快照，快照时钟比渲染时钟更一致）；在事件回调里取时满足
      // react-hooks/purity——render 期不得调用 Date.now() 这类 impure 函数
      setNow(Date.now());
      setLoadError(false);
    } catch (e) {
      console.error("hook_signal_health failed:", e);
      setLoadError(true);
    } finally {
      setLoading(false);
    }
  }, []);

  // 挂载即拉取（无自动轮询：健康自查是按需动作，设置页打开/手动刷新各一次）
  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 复制 /hooks（信任门一次性操作入口；剪贴板不可用 → toast 报错，同 RemoteSection 口径）
  const copyCommand = async () => {
    try {
      await navigator.clipboard.writeText("/hooks");
      toast.success(t("settings.signalHealth.copied"));
    } catch (e) {
      console.error("clipboard write failed:", e);
      toast.error(t("settings.signalHealth.copyFailed"));
    }
  };

  // 相对时间（lastEventAt 为 RFC3339 UTC；档位：秒/分/时/天）。
  // now 仅在 refresh 成功后置位，绿态行只在 items 非空（必经 refresh）时渲染，
  // ?? 0 分支实际不可达（退化为 0s，防类型空洞）
  const ago = (iso: string) => {
    const secs = Math.max(0, Math.floor(((now ?? 0) - new Date(iso).getTime()) / 1000));
    if (secs < 60) return t("settings.signalHealth.secondsAgo", { n: secs });
    if (secs < 3600) return t("settings.signalHealth.minutesAgo", { n: Math.floor(secs / 60) });
    if (secs < 86400) return t("settings.signalHealth.hoursAgo", { n: Math.floor(secs / 3600) });
    return t("settings.signalHealth.daysAgo", { n: Math.floor(secs / 86400) });
  };

  return (
    <div className="overflow-hidden rounded-xl border">
      {/* 卡头条（2026-10-11 两级标题拉开）：h2 保留档位常量搬进条内；说明收条右侧 */}
      <div className="bg-muted/40 flex items-center justify-between gap-2 border-b px-4 py-2.5">
        <h2 className={SETTINGS_CARD_TITLE}>{t("settings.signalHealth.title")}</h2>
        <div className="flex min-w-0 items-baseline gap-2">
          <span className="text-muted-foreground truncate text-xs">
            {t("settings.signalHealth.description")}
          </span>
          <Button variant="outline" size="sm" onClick={() => void refresh()} disabled={loading}>
            {t("settings.signalHealth.refresh")}
          </Button>
        </div>
      </div>

      <div className="p-3">
        {items.length === 0 ? (
          loadError ? (
            <p className="text-sm text-red-500" data-testid="signal-health-load-error">
              {t("settings.signalHealth.loadError")}
            </p>
          ) : (
            <p className="text-muted-foreground text-sm">{t("settings.signalHealth.empty")}</p>
          )
        ) : (
          <div className="divide-border divide-y">
            {items.map((h) => (
              <div
                key={h.toolId}
                data-signal-row={h.toolId}
                className="flex items-center justify-between gap-3 px-3 py-2"
              >
                <div className="flex min-w-0 flex-wrap items-center gap-2">
                  <ToolIcon toolId={h.toolId} size={16} />
                  <span className={SETTINGS_FIELD}>{h.label}</span>
                  {!h.registered ? (
                    // 灰：未注册。1A 细分：helperNotShipped = 本平台不随附 Hook 通道的
                    // 设计内状态（如 macOS 上的 kimi）→ 蓝 badge 解释性文案替代裸
                    // 「未注册」，用户无需任何处理；其余维持裸「未注册」待用户排查
                    h.unregisteredReason === "helperNotShipped" ? (
                      <span className="rounded bg-blue-500/15 px-1.5 py-0.5 text-[10px] text-blue-600 dark:text-blue-400">
                        {t("settings.signalHealth.helperNotShipped")}
                      </span>
                    ) : (
                      <span className="bg-muted text-muted-foreground rounded px-1.5 py-0.5 text-[10px]">
                        {t("settings.signalHealth.statusUnregistered")}
                      </span>
                    )
                  ) : h.lastEventAt ? (
                    // 绿：正常 · 最近事件 xx 前
                    <span className="rounded bg-emerald-500/10 px-1.5 py-0.5 text-[10px] text-emerald-500">
                      {t("settings.signalHealth.ok", { ago: ago(h.lastEventAt) })}
                    </span>
                  ) : h.hasActiveSessions ? (
                    // 待办：判据命中（已注册 ∧ 活跃会话 ∧ 零事件）。文案按工具区分：
                    // codex 有信任门（注册成功 ≠ 事件触发）→ 专属引导 + 复制 /hooks；
                    // 其余工具无信任门 → 通用零事件排查文案（复制 /hooks 无意义，不显示）
                    <>
                      <span className="rounded bg-amber-500/10 px-1.5 py-0.5 text-[10px] text-amber-500">
                        {h.toolId === "codex"
                          ? t("settings.signalHealth.trustNeeded", { tool: h.label })
                          : t("settings.signalHealth.zeroEvents", { tool: h.label })}
                      </span>
                      {h.toolId === "codex" && (
                        <Button variant="outline" size="sm" onClick={() => void copyCommand()}>
                          <Copy className="mr-1 h-3 w-3" />
                          {t("settings.signalHealth.copyCommand")}
                        </Button>
                      )}
                    </>
                  ) : (
                    // 中性：已注册但无活跃会话（零事件属正常等待，不算故障）
                    <span className="bg-muted text-muted-foreground rounded px-1.5 py-0.5 text-[10px]">
                      {t("settings.signalHealth.noActiveSessions")}
                    </span>
                  )}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

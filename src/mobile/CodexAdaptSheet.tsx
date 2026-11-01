/**
 * F08／T08-T10：**适配详情与开关**（移动端）。
 *
 * 只读展示系统/Tuvis/Desktop 版本、逐能力证据与最近失败；开关与桌面设置同源
 * （`/codex/adapt-settings`）。首次提示只在 `hintSeen=false` 时出现，用户点"知道了"
 * 后由服务端记住（不随 build 重置）。
 *
 * R07（能力事实同源）：**能力表只渲染页面同版的 `effectiveOperations`**（工具栏同一
 * 份投影事实）——不读独立 GET 的 `effectiveOperations`。独立 GET
 * （`fetchCodexAdaptStatus`）只补版本、自动适配开关、首次提示与诊断/最近失败，
 * 因此能力表不会因为打开时那一瞬的旧快照而长期过期，也不受该 GET 失败影响。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import {
  fetchCodexAdaptStatus,
  supportOf,
  updateCodexAdaptSettings,
  type CodexAdaptDetails,
  type CodexEffectiveOperations,
} from "./codexIpcApi";
import { codexReasonText, codexSupportText } from "./CodexOperationToolbar";

export interface CodexAdaptSheetProps {
  sessionId: string;
  open: boolean;
  onClose: () => void;
  /** 页面同版能力事实（工具栏消费的同一份投影 `effectiveOperations`）。
   *  R07：能力表**只**渲染它；缺省 = 页面还没给出事实 → 不渲染能力表
   *  （不用独立 GET 的旧快照冒充）。 */
  effectiveOperations?: CodexEffectiveOperations;
}

export default function CodexAdaptSheet({
  sessionId,
  open,
  onClose,
  effectiveOperations,
}: CodexAdaptSheetProps) {
  const [details, setDetails] = useState<CodexAdaptDetails | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const load = useCallback(async () => {
    setError(null);
    try {
      const value = await fetchCodexAdaptStatus(sessionId);
      if (alive.current) setDetails(value);
    } catch (e) {
      if (alive.current) setError(i18n.t("mobile.codex.adapt.loadFailed"));
      void e;
    }
  }, [sessionId]);

  useEffect(() => {
    if (open) void load();
  }, [open, load]);

  const toggle = useCallback(async () => {
    if (!details || busy) return;
    setBusy(true);
    setError(null);
    try {
      const next = await updateCodexAdaptSettings({
        sessionId,
        action: "set_enabled",
        enabled: !details.autoAdaptEnabled,
      });
      if (alive.current) setDetails(next);
    } catch {
      if (alive.current) setError(i18n.t("mobile.codex.adapt.toggleFailed"));
    } finally {
      if (alive.current) setBusy(false);
    }
  }, [details, busy, sessionId]);

  const dismissHint = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      const next = await updateCodexAdaptSettings({ sessionId, action: "dismiss_hint" });
      if (alive.current) setDetails(next);
    } catch {
      if (alive.current) setError(i18n.t("mobile.codex.adapt.toggleFailed"));
    } finally {
      if (alive.current) setBusy(false);
    }
  }, [busy, sessionId]);

  if (!open) return null;

  return (
    <div
      data-testid="codex-adapt-sheet"
      role="dialog"
      aria-label={i18n.t("mobile.codex.adapt.title")}
      className="rounded-xl border border-[var(--bd)] bg-[var(--cd)] p-3 text-sm"
    >
      <header className="mb-2 flex items-center justify-between">
        <span className="font-medium">{i18n.t("mobile.codex.adapt.title")}</span>
        <button type="button" onClick={onClose} data-testid="codex-adapt-close">
          ×
        </button>
      </header>
      {error && (
        <p role="alert" className="mb-2 text-xs text-red-600">
          {error}
        </p>
      )}
      {details && (
        <dl className="mb-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
          <dt className="text-[var(--mut)]">{i18n.t("mobile.codex.adapt.system")}</dt>
          <dd data-testid="codex-adapt-system">{details.systemVersion}</dd>
          <dt className="text-[var(--mut)]">{i18n.t("mobile.codex.adapt.tuvis")}</dt>
          <dd data-testid="codex-adapt-tuvis">{details.tuvisVersion}</dd>
          <dt className="text-[var(--mut)]">{i18n.t("mobile.codex.adapt.desktop")}</dt>
          <dd data-testid="codex-adapt-desktop">{details.desktopBuild ?? "—"}</dd>
          <dt className="text-[var(--mut)]">{i18n.t("mobile.codex.adapt.autoAdapt")}</dt>
          <dd>
            <button
              type="button"
              role="switch"
              aria-checked={details.autoAdaptEnabled}
              disabled={busy}
              onClick={() => void toggle()}
              data-testid="codex-adapt-toggle"
              className="rounded-full border border-[var(--bd)] px-2 py-0.5"
            >
              {details.autoAdaptEnabled ? "ON" : "OFF"}
            </button>
          </dd>
          <dt className="text-[var(--mut)]">{i18n.t("mobile.codex.adapt.latestFailure")}</dt>
          <dd data-testid="codex-adapt-failure">
            {details.lastFailure
              ? `${details.lastFailure.capability} · ${details.lastFailure.errorCode ?? details.lastFailure.state}`
              : i18n.t("mobile.codex.adapt.noFailure")}
          </dd>
        </dl>
      )}
      {effectiveOperations && (
        <ul data-testid="codex-adapt-ops" className="mt-2 space-y-1 text-xs">
          {(
            Object.entries(effectiveOperations.operations) as [
              string,
              NonNullable<
                CodexEffectiveOperations["operations"][keyof CodexEffectiveOperations["operations"]]
              >,
            ][]
          )
            .filter(([, status]) => status.implemented)
            .map(([operation, status]) => (
              <li
                key={operation}
                data-testid={`codex-adapt-op-${operation}`}
                data-allowed={status.allowed ? "true" : "false"}
                data-support={supportOf(status)}
                className="flex flex-wrap items-center gap-x-2 gap-y-0.5"
              >
                <span className="font-medium">{operation}</span>
                {/* T03：支持状态（稳定展示）与当前资格（allowed + 原因）分开说。 */}
                <span className="text-[var(--mut)]">{codexSupportText(supportOf(status))}</span>
                <span className={status.allowed ? "text-emerald-600" : "text-[var(--mut)]"}>
                  {status.allowed
                    ? i18n.t("mobile.codex.ops.allowed")
                    : i18n.t("mobile.codex.ops.blocked")}
                </span>
                <span className="text-[var(--mut)]">
                  {i18n.t("mobile.codex.ops.evidence")}: {status.evidence}
                </span>
                {status.reasonCode && (
                  <span className="text-[var(--mut)]">{codexReasonText(status.reasonCode)}</span>
                )}
              </li>
            ))}
        </ul>
      )}
      {details && !details.hintSeen && (
        <div data-testid="codex-adapt-hint" className="mt-2 rounded-lg bg-[var(--cb)] p-2 text-xs">
          <p>{i18n.t("mobile.codex.adapt.hint")}</p>
          <button
            type="button"
            data-testid="codex-adapt-hint-ok"
            onClick={() => void dismissHint()}
            className="mt-1 underline"
          >
            {i18n.t("mobile.codex.adapt.hintOk")}
          </button>
        </div>
      )}
    </div>
  );
}

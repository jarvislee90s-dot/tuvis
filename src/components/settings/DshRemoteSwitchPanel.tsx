// 「远程控制 DSH」单开关面板（Task 4；DSH Spec §6/§6.1 主按钮全生命周期）。
//
// **消费面（只消费不另造）**：
// - 开关状态机 = `dsh_switch_get/enable/disable/repair`（`useDshSwitchQuery`）——
//   后端按选定实例持久化开关与推进状态，本组件只发起意图与渲染实际状态；
//   线性推进（等待退出 → 安装 → 重开 → 核验 → 授权）全部由后端驱动，跨设置页
//   与兔维斯重启可恢复；
// - 重开入口 = `dsh_restart_host`（受控重开：只启动所选实例的 Desktop 应用）；
// - 重绑确认 = `dsh_binding_start`（双端核对面）+ `dsh_binding_rebind`（既有
//   可信重绑机制；仅在 rebind_required 时出现，身份变化不得无提示信任）；
// - 修复插件 = `dsh_switch_repair`（只作用于当前实例，修复完成后由后端自动
//   接续原流程）；常规界面不再有独立的安装/绑定/撤权/卸载按钮（R2 开关门）；
// - 发送/控制台就绪结果 = 后端实时判定（`send` / `console` 三态），已保存授权
//   与当前已连接分开呈现。
import { useEffect, useRef, useState } from "react";
import { ChevronDown } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { cn } from "@/lib/utils";
import { formatInvokeError } from "@/lib/invokeError";
import {
  dshBindingRebind,
  dshBindingStart,
  dshRestartHost,
  dshSwitchDisable,
  dshSwitchEnable,
  dshSwitchRepair,
} from "@/lib/api/dsh";
import { isDshJobActive, useDshSwitchQuery } from "@/lib/query/queries/dsh";
import type {
  DshBindingStartResult,
  DshChannelReadiness,
  DshManagementPhase,
  DshSwitchPhase,
  DshSwitchStep,
  DshSwitchView,
} from "@/types/dsh";

type TFunc = (key: string, options?: Record<string, unknown>) => string;

/** 错误码 → 用户文案（`errors.<camelCase(码)>` 由 check-error-codes 门禁保证
 *  zh/en 双键齐备；未登记码回退原码展示——不静默吞）。 */
function reasonText(code: string | null, t: TFunc): string | null {
  if (!code) return null;
  const key = `errors.${code
    .toLowerCase()
    .split("_")
    .map((w, i) => (i === 0 ? w : w.charAt(0).toUpperCase() + w.slice(1)))
    .join("")}`;
  const text = t(key);
  return text === key ? code : text;
}

/** 步骤文案键族（dshStep-前缀 + 步骤词；与 locales 的同名键对账）。 */
const STEP_LABEL: Record<DshSwitchStep, string> = {
  instance_check: "dshStep_instance_check",
  install: "dshStep_install",
  reopen: "dshStep_reopen",
  verify: "dshStep_verify",
  authorize: "dshStep_authorize",
  repair: "dshStep_repair",
  revoke: "dshStep_revoke",
  uninstall: "dshStep_uninstall",
  home_evidence: "dshStep_home_evidence",
};

/** 开启方向线性步（phase → 步序；「按需等待 DSH 退出」并入安装步的作业面）。 */
const ENABLE_STEPS: { step: DshSwitchStep; phases: DshSwitchPhase[] }[] = [
  { step: "instance_check", phases: ["checking"] },
  { step: "install", phases: ["installing"] },
  { step: "reopen", phases: ["awaiting_reopen"] },
  { step: "verify", phases: ["verifying_plugin"] },
  { step: "authorize", phases: ["authorizing", "rebind_required"] },
];

function stepState(phase: DshSwitchPhase, index: number): "done" | "active" | "pending" {
  if (phase === "ready") return "done";
  const activeIndex = ENABLE_STEPS.findIndex((step) => step.phases.includes(phase));
  if (activeIndex < 0) return "pending";
  if (index < activeIndex) return "done";
  if (index === activeIndex) return "active";
  return "pending";
}

/** 作业阶段文案（复用既有 dshPhase* 键；等待退出的指引在作业面内单独呈现）。 */
const PHASE_LABEL: Record<DshManagementPhase, string> = {
  queued: "settings.remote.dshPhaseQueued",
  waiting_for_desktop_exit: "settings.remote.dshPhaseWaiting",
  applying: "settings.remote.dshPhaseApplying",
  verifying: "settings.remote.dshPhaseVerifying",
  succeeded: "settings.remote.dshPhaseSucceeded",
  failed: "settings.remote.dshPhaseFailed",
  cancelled: "settings.remote.dshPhaseCancelled",
};

type Readiness = DshChannelReadiness | "off" | "unknown" | "unverified";

const READINESS_LABEL: Record<Readiness, string> = {
  ready: "settings.remote.dshReadinessReady",
  recovering: "settings.remote.dshReadinessRecovering",
  not_authorized: "settings.remote.dshReadinessNotAuthorized",
  off: "settings.remote.dshReadinessOff",
  unknown: "settings.remote.dshStatusUnknown",
  unverified: "settings.remote.dshConsolePageUnverified",
};

const PLUGIN_STATUS_ROWS = {
  installation: "settings.remote.dshStatus_installation",
  activation: "settings.remote.dshStatus_activation",
  authorization: "settings.remote.dshStatus_authorization",
} as const;

const PLUGIN_FACT_LABEL = {
  missing: "settings.remote.dshFact_missing",
  installed: "settings.remote.dshFact_installed",
  conflict: "settings.remote.dshFact_conflict",
  broken: "settings.remote.dshFact_broken",
  disabled: "settings.remote.dshFact_disabled",
  enabled: "settings.remote.dshFact_enabled",
  restart_required: "settings.remote.dshFact_restart_required",
  running: "settings.remote.dshFact_running",
  unbound: "settings.remote.dshFact_unbound",
  bound: "settings.remote.dshFact_bound",
  revoked: "settings.remote.dshFact_revoked",
} as const;

function ReadinessRow({
  label,
  value,
  testid,
  t,
}: {
  label: string;
  value: Readiness;
  testid: string;
  t: TFunc;
}) {
  const tone =
    value === "ready"
      ? "text-emerald-600 dark:text-emerald-400"
      : value === "recovering"
        ? "text-amber-600 dark:text-amber-400"
        : "text-muted-foreground";
  return (
    <p className="flex items-center gap-1.5 text-xs">
      <span className="text-muted-foreground">{label}</span>
      <span data-testid={testid} data-state={value} className={tone}>
        {t(READINESS_LABEL[value])}
      </span>
    </p>
  );
}

export function DshRemoteSwitchPanel({
  instanceId,
  visible,
  displayName,
}: {
  instanceId: string | null;
  visible: boolean;
  displayName: string | null;
}) {
  const { t } = useAppTranslation();
  const switchQ = useDshSwitchQuery(instanceId, visible && instanceId !== null);
  const view: DshSwitchView | null = switchQ.data ?? null;
  const [actionError, setActionError] = useState<string | null>(null);
  const [mutating, setMutating] = useState(false);
  const [confirmEnable, setConfirmEnable] = useState(false);
  const [repairOpen, setRepairOpen] = useState(false);
  // 重绑面板（rebind_required 时的双端核对面；身份变化必须用户明确确认）
  const [rebind, setRebind] = useState<DshBindingStartResult | null>(null);
  const epoch = useRef(0);

  useEffect(() => {
    // 实例切换 / 查询出错：丢弃上一实例的重绑面板（旧意图不跨实例）。
    epoch.current += 1;
    setRebind(null);
    setActionError(null);
  }, [instanceId]);

  useEffect(() => {
    if (switchQ.isError) {
      epoch.current += 1;
      setRebind(null);
    }
  }, [switchQ.isError, switchQ.error]);

  useEffect(() => {
    if (view?.rebindRequired && rebind === null && !mutating) {
      // 进入重绑等待：拉一次双端核对面（旧/新指纹 + 核对码）。
      const version = ++epoch.current;
      void (async () => {
        try {
          const started = await dshBindingStart();
          if (version === epoch.current) setRebind(started);
        } catch {
          // 面板数据暂不可得：保持 rebind_required 呈现，不猜。
        }
      })();
    }
    if (view !== null && !view.rebindRequired && rebind !== null) {
      setRebind(null);
    }
  }, [view, rebind, mutating]);

  const invalidate = async () => {
    await switchQ.refetch();
  };

  const enable = async () => {
    if (!instanceId || mutating) return;
    setMutating(true);
    setActionError(null);
    try {
      await dshSwitchEnable(instanceId);
      await invalidate();
    } catch (e) {
      setActionError(formatInvokeError(e, t));
    } finally {
      setMutating(false);
    }
  };

  const disable = async () => {
    if (!instanceId || mutating) return;
    setMutating(true);
    setActionError(null);
    try {
      await dshSwitchDisable(instanceId);
      await invalidate();
    } catch (e) {
      setActionError(formatInvokeError(e, t));
    } finally {
      setMutating(false);
    }
  };

  const repair = async () => {
    if (!instanceId || mutating) return;
    setMutating(true);
    setActionError(null);
    try {
      await dshSwitchRepair(instanceId);
      await invalidate();
    } catch (e) {
      setActionError(formatInvokeError(e, t));
    } finally {
      setMutating(false);
    }
  };

  const confirmRebind = async () => {
    if (
      !rebind?.instanceId ||
      !rebind.oldFingerprint ||
      !rebind.newFingerprint ||
      rebind.oldFingerprint === rebind.newFingerprint ||
      rebind.instanceId !== instanceId ||
      mutating
    )
      return;
    setMutating(true);
    setActionError(null);
    try {
      await dshBindingRebind(rebind.instanceId, rebind.bindingId, rebind.oldFingerprint);
      setRebind(null);
      await invalidate();
    } catch (e) {
      setActionError(formatInvokeError(e, t));
    } finally {
      setMutating(false);
    }
  };

  const restartHost = async () => {
    if (!instanceId || mutating) return;
    setMutating(true);
    setActionError(null);
    try {
      await dshRestartHost(instanceId);
      await invalidate();
    } catch (e) {
      setActionError(formatInvokeError(e, t));
    } finally {
      setMutating(false);
    }
  };

  const phase = view?.phase ?? null;
  const job = view?.job ?? null;
  const jobActive = job !== null && isDshJobActive(job.phase);
  const busy = mutating || jobActive;
  const checked = view?.enabled ?? false;
  // 关闭未收敛（与后端 `SwitchRecord::close_pending` **逐字同源**）：`waiting_uninstall`
  // 无条件算关闭未收敛（相位本身已隐含关闭方向），或关闭侧失败（`enabled=false`
  // 且失败步骤为撤权/卸载/撤权前 home 证据回填）。这些状态下旧绑定/旧钥尚未安全
  // 退役——总开关直接开启会被后端拒绝，「修复插件」也不得绕过，唯一恢复动作是
  // 「重试关闭」（再次走 disable 收敛）。判据必须由真实开关记录得出，不能靠作业
  // 相位猜（遗留 jobId 不是活跃作业）。
  const closePending =
    view !== null &&
    (phase === "waiting_uninstall" ||
      (phase === "failed" &&
        !view.enabled &&
        (view.failedStep === "revoke" ||
          view.failedStep === "uninstall" ||
          view.failedStep === "home_evidence")));
  // 关闭未收敛时禁止开启。「修复插件」关闭的不是唯一被禁的入口——它同样不得
  // 绕过未完成的关闭（修复完成会把记录洗成可推进态，旧钥随复活）。
  // 开关可交互（P1-2 等待期可关闭）：推进中（含安装作业等待 DSH 退出）保持可点
  // ——关闭随时可达（后端先取消进行中的作业再走关闭段）；关闭未收敛期间禁止
  // 直接开启（唯一出口是「重试关闭」）；仅修复中锁定（修复完成自动接续原流程，
  // 中途关闭会让接续语义悬空）。
  const switchEnabled =
    instanceId !== null && !mutating && view !== null && phase !== "repairing" && !closePending;
  // 首次开启确认（最多一次，放在开始前）：开关当前处于「未启用」事实态；等待
  // 卸载期间的再开启必被后端拒绝，跳过确认框让拒绝原因直接上屏。
  const showConfirm =
    view !== null && !view.enabled && phase !== "failed" && phase !== "waiting_uninstall";
  // 失败态重试路由按 enabled 字段判向：关闭侧 Failed（撤权/卸载失败，
  // enabled=false）只会路由到 disable()（重试关闭收敛）——后端第九轮 P1-1 起
  // 对 close_pending 态的 enable/repair/直调一律拒绝，此判向是前端唯一正确出口。
  const failedEnable = phase === "failed" && (view?.enabled ?? false);
  const failedDisable = phase === "failed" && !(view?.enabled ?? true);
  const repairJob = phase === "repairing";
  const plugin = switchQ.isError ? null : view?.pluginStatus;
  const readiness = (state: DshChannelReadiness | null, console = false): Readiness => {
    if (switchQ.isError || view === null) return "unknown";
    if (!view.enabled) return "off";
    if (console && state === "ready") return "unverified";
    // 有连接不代表开关推进已完成，也不等于移动页能力放行。
    if (phase !== "ready" && state === "ready") return "recovering";
    return state ?? "unknown";
  };

  if (instanceId === null) {
    return (
      <p className="text-muted-foreground mt-2 text-xs">{t("settings.remote.dshSelectFirst")}</p>
    );
  }

  return (
    <div className="mt-2 space-y-1.5">
      {/* 开关行：主按钮 = 本次安装 + 实例级授权意图 */}
      <div className="flex items-center justify-between gap-4 py-1">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.dshSwitchLabel")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.dshSwitchHint")}
            {displayName !== null && <span className="ml-1 font-medium">{displayName}</span>}
          </p>
          {view?.reasonCode != null && (
            <p className="text-muted-foreground mt-0.5 text-xs">{reasonText(view.reasonCode, t)}</p>
          )}
          {switchQ.isError && (
            <p className="mt-0.5 text-xs text-rose-600 dark:text-rose-400">
              {formatInvokeError(switchQ.error, t)}
            </p>
          )}
        </div>
        <Switch
          data-testid="dsh-switch"
          aria-label={t("settings.remote.dshSwitchLabel")}
          checked={checked}
          disabled={!switchEnabled}
          onCheckedChange={(next) => {
            if (next) {
              if (showConfirm) setConfirmEnable(true);
              else void enable();
            } else {
              void disable();
            }
          }}
        />
      </div>

      {/* 线性推进（开启方向；事实就绪的步骤由后端自动跳过） */}
      {view !== null && (view.enabled || phase === "ready") && phase !== "waiting_uninstall" && (
        <div data-testid="dsh-switch-steps" className="space-y-1 rounded-lg border p-3 text-xs">
          {ENABLE_STEPS.map((entry, index) => {
            const state = stepState(phase ?? "checking", index);
            return (
              <p
                key={entry.step}
                data-testid={`dsh-step-${entry.step}`}
                data-state={state}
                className={cn(
                  "flex items-center gap-1.5",
                  state === "active"
                    ? "text-foreground font-medium"
                    : state === "done"
                      ? "text-emerald-600 dark:text-emerald-400"
                      : "text-muted-foreground"
                )}
              >
                {state === "done" ? "✓" : state === "active" ? "•" : "○"}
                {t(`settings.remote.${STEP_LABEL[entry.step]}`)}
              </p>
            );
          })}
          {phase === "installing" && job?.phase === "waiting_for_desktop_exit" && jobActive && (
            <p data-testid="dsh-waiting-exit" className="text-muted-foreground">
              {t("settings.remote.dshPhaseWaitingHint")}
            </p>
          )}
          {phase === "awaiting_reopen" && (
            <div className="flex items-center gap-2">
              <p data-testid="dsh-await-reopen" className="text-muted-foreground">
                {t("settings.remote.dshAwaitReopenHint")}
              </p>
              <Button
                size="sm"
                variant="outline"
                className="h-6 px-2 text-xs"
                data-testid="dsh-reopen-host"
                disabled={busy}
                onClick={() => void restartHost()}
              >
                {t("settings.remote.dshRestartNow")}
              </Button>
            </div>
          )}
          {phase === "rebind_required" && (
            <p data-testid="dsh-rebind-required" className="text-amber-600 dark:text-amber-400">
              {t("settings.remote.dshRebindRequiredHint")}
            </p>
          )}
        </div>
      )}

      {view !== null && (
        <div data-testid="dsh-readiness" className="space-y-1 rounded-lg border p-3 text-xs">
          <dl className="space-y-0.5">
            {(
              [
                ["installation", plugin?.installation],
                ["activation", plugin?.activation],
                ["authorization", plugin?.authorization],
              ] as const
            ).map(([kind, value]) => (
              <div key={kind} className="flex items-center gap-1.5">
                <dt className="text-muted-foreground">{t(PLUGIN_STATUS_ROWS[kind])}</dt>
                <dd data-testid={`dsh-${kind}-state`} data-state={value ?? "unknown"}>
                  {t(value ? PLUGIN_FACT_LABEL[value] : "settings.remote.dshStatusUnknown")}
                  {kind === "installation" && plugin?.version && (
                    <span className="text-muted-foreground ml-1">{plugin.version}</span>
                  )}
                </dd>
              </div>
            ))}
          </dl>
          {plugin?.reasonCode && plugin.reasonCode !== view.reasonCode && (
            <p className="text-muted-foreground">{reasonText(plugin.reasonCode, t)}</p>
          )}
          <ReadinessRow
            label={t("settings.remote.dshChannelSend")}
            value={readiness(view.send)}
            testid="dsh-send-readiness"
            t={t}
          />
          <ReadinessRow
            label={t("settings.remote.dshChannelConsole")}
            value={readiness(view.console, true)}
            testid="dsh-console-readiness"
            t={t}
          />
        </div>
      )}

      {/* 远控已关闭 · 等待卸载（关闭方向自动推进；等待宿主正常退出） */}
      {view !== null && !view.enabled && phase !== "off" && phase !== "failed" && (
        <div
          data-testid="dsh-waiting-uninstall"
          className="bg-muted/30 space-y-1 rounded-lg border p-3 text-xs"
        >
          <p className="font-medium">{t("settings.remote.dshWaitingUninstall")}</p>
          <p className="text-muted-foreground" data-testid="dsh-close-waiting-hint">
            {job?.phase === "waiting_for_desktop_exit"
              ? t("settings.remote.dshCloseWaitingHint")
              : t("settings.remote.dshClosePendingHint")}
          </p>
          {job !== null && (
            <>
              <p
                className="text-muted-foreground"
                data-testid="dsh-uninstall-phase"
                data-phase={job.phase}
              >
                {t(PHASE_LABEL[job.phase])}
              </p>
              {/* 作业登记的具体原因如实展示：归属不明（INSTANCE_UNVERIFIED）等
                  等待原因必须可见，不能让用户只看到「等待」却不知为何。 */}
              {job.reasonCode !== null && job.reasonCode !== view.reasonCode && (
                <p className="text-muted-foreground" data-testid="dsh-uninstall-reason">
                  {reasonText(job.reasonCode, t)}
                </p>
              )}
            </>
          )}
        </div>
      )}

      {/* 失败：意图保持（开启失败保持开意图、关闭失败保持关意图），失败步骤 + 重试 */}
      {view !== null && phase === "failed" && (
        <div
          data-testid="dsh-failed"
          className="space-y-1 rounded-lg border border-rose-500/30 p-3 text-xs"
        >
          <p className="font-medium text-rose-600 dark:text-rose-400">
            {failedDisable
              ? t("settings.remote.dshDisableFailed")
              : t("settings.remote.dshEnableFailed")}
            {view.failedStep !== null && (
              <span className="text-muted-foreground ml-1 font-normal">
                {t(`settings.remote.${STEP_LABEL[view.failedStep]}`)}
              </span>
            )}
          </p>
          {view.reasonCode !== null && (
            <p className="text-muted-foreground">{reasonText(view.reasonCode, t)}</p>
          )}
          {closePending && (
            <p className="text-muted-foreground" data-testid="dsh-close-pending-hint">
              {failedDisable
                ? t("settings.remote.dshClosePendingHint")
                : t("settings.remote.dshCloseWaitingHint")}
            </p>
          )}
          <Button
            size="sm"
            variant="outline"
            className="h-6 px-2 text-xs"
            data-testid="dsh-retry"
            disabled={busy}
            onClick={() => (failedEnable ? void enable() : void disable())}
          >
            {failedDisable ? t("settings.remote.dshRetryClose") : t("settings.remote.dshRetry")}
          </Button>
        </div>
      )}

      {/* 重绑面板（既有可信重绑机制；身份变化不得无提示信任） */}
      {view?.rebindRequired === true && rebind !== null && (
        <div
          data-testid="dsh-binding-panel"
          className="bg-muted/30 space-y-1.5 rounded-lg border p-3 text-xs"
        >
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t("settings.remote.dshCompareCode")}</span>
            <span
              data-testid="dsh-display-code"
              className="bg-background rounded px-2 py-0.5 font-mono text-sm font-semibold tracking-widest"
            >
              {rebind.displayCode}
            </span>
          </div>
          <p className="text-muted-foreground">
            {t("settings.remote.dshBindingPeerLabel")}{" "}
            <span data-testid="dsh-binding-peer">
              {rebind.clientExecutable ?? "?"} · PID {rebind.clientPid ?? "?"}
            </span>
          </p>
          <p className="text-muted-foreground">
            {t("settings.remote.dshRustFingerprint")}{" "}
            <span className="font-mono">{rebind.rustFingerprint}</span>
          </p>
          {rebind.oldFingerprint && rebind.newFingerprint && (
            <div className="space-y-1 break-all">
              <p>
                {t("settings.remote.dshOldFingerprint")}{" "}
                <span className="font-mono">{rebind.oldFingerprint}</span>
              </p>
              <p>
                {t("settings.remote.dshNewFingerprint")}{" "}
                <span className="font-mono">{rebind.newFingerprint}</span>
              </p>
              <p>{t("settings.remote.dshRebindHint")}</p>
            </div>
          )}
          <div className="flex gap-2">
            <Button
              size="sm"
              className="h-7 px-2 text-xs"
              data-testid="dsh-confirm-rebind"
              disabled={mutating}
              onClick={() => void confirmRebind()}
            >
              {t("settings.remote.dshConfirmRebind")}
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-7 px-2 text-xs"
              data-testid="dsh-dismiss-binding"
              onClick={() => setRebind(null)}
            >
              {t("settings.remote.dshDismissBinding")}
            </Button>
          </div>
        </div>
      )}

      {/* 修复插件（常规界面唯一保留的管理入口；只作用于当前实例） */}
      <div className="pt-1">
        <button
          type="button"
          data-testid="dsh-repair-toggle"
          aria-expanded={repairOpen}
          onClick={() => setRepairOpen((v) => !v)}
          className="text-muted-foreground flex items-center gap-1 text-xs font-medium hover:underline"
        >
          <ChevronDown
            size={14}
            className={cn("transition-transform", repairOpen ? "rotate-180" : undefined)}
          />
          {t("settings.remote.dshRepairEntry")}
        </button>
        {repairOpen && (
          <div className="mt-1 flex items-center gap-2 text-xs">
            <p className="text-muted-foreground">{t("settings.remote.dshRepairHint")}</p>
            <Button
              size="sm"
              variant="outline"
              className="h-6 px-2 text-xs"
              data-testid="dsh-repair-start"
              disabled={busy || repairJob || closePending}
              onClick={() => void repair()}
            >
              {repairJob
                ? t("settings.remote.dshRepairRunning")
                : t("settings.remote.dshRepairEntry")}
            </Button>
            {repairJob && (
              <span data-testid="dsh-repair-active" className="text-amber-600 dark:text-amber-400">
                {t("settings.remote.dshRepairRunning")}
              </span>
            )}
          </div>
        )}
      </div>

      {/* 动作错误如实显示 */}
      {actionError !== null && (
        <p data-testid="dsh-error" className="text-xs text-rose-600 dark:text-rose-400">
          {actionError}
        </p>
      )}

      {/* 首次开启确认（最多一次，放在开始前；取消 = 不开始作业） */}
      <Dialog open={confirmEnable} onOpenChange={setConfirmEnable}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t("settings.remote.dshEnableConfirmTitle")}</DialogTitle>
            <DialogDescription>{t("settings.remote.dshEnableConfirmBody")}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              data-testid="dsh-enable-confirm-cancel"
              onClick={() => setConfirmEnable(false)}
            >
              {t("settings.remote.dshEnableConfirmCancel")}
            </Button>
            <Button
              data-testid="dsh-enable-confirm-ok"
              disabled={mutating}
              onClick={() => {
                setConfirmEnable(false);
                void enable();
              }}
            >
              {t("settings.remote.dshEnableConfirmOk")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

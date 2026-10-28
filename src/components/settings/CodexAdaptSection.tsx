// Codex 自动适配卡（S5）：设置页「自动适配」开关 + 可点击状态详情。
//
// **消费面（只消费不另造）**：
// - 开关真值 = `remote_status.codexAutoAdaptEnabled/Explicit`（S1 设置键
//   `remote.codex_auto_adapt` 的生效值；命令 `remote_toggle_codex_auto_adapt`）；
// - 状态详情 = Tauri 命令 `codex_adapt_status`（`CodexAdaptStatus` DTO）——前端只做
//   展示与动作发起，不复刻任何能力策略；
// - 四态映射（S5 简报裁决）：已适配=有证据（evidenced）；检查中=探测在飞；
//   连接异常=暂不可尝试且非关闭态；已关闭=显式关。开启且可尝试但尚无证据 →
//   「待检查」（连接后自动检查）——不宣称已解锁全部能力；
// - 首次轻提示一次语义（设计 §3.4）：已阅读记录在通用设置 KV
//   `remote.codex_auto_adapt_hint_seen`，不随 build 重复弹出；
// - 报告入口（R5 前身）：`src/lib/reportIssue` 统一事件，payload 只含安全引用；
//   诊断服务未部署 → 明确「诊断服务未开启」+ 复制摘要出口，不伪造成功。
import { useCallback, useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { toast } from "sonner";
import { cn } from "@/lib/utils";
import { formatInvokeError } from "@/lib/invokeError";
import { getSetting, setSetting } from "@/lib/api/settings";
// 组标题档（线稿 .group-label 硬契约）：非豁免面不得写裸字号字面量（排版锁 ③）
import { SETTINGS_REMOTE_GROUP_LABEL } from "@/components/settings/typography";
import { adaptSummaryOf, codexAdaptStatus, type CodexAdaptStatus } from "@/lib/api/remote";
import { reportIssue, reportIssueSummary } from "@/lib/reportIssue";

/** 首次轻提示「已阅读」记录键（通用设置 KV；写 "true" = 已读过）。 */
export const CODEX_ADAPT_HINT_KEY = "remote.codex_auto_adapt_hint_seen";

/** 详情区四态（+ unknown：DTO 取不到时如实呈现，不猜）。 */
export type CodexAdaptUiState = "off" | "probing" | "adapted" | "error" | "checking" | "unknown";

/**
 * 四态映射（纯函数，唯一映射源——徽标与测试同源）：
 * - off：显式关（用户写过的 false；缺省开不算关）；
 * - probing：探测在飞（结论未定不抢跑）；
 * - adapted：有**已取证**证据（evidenced 任一位——不随断链回落）；
 * - error：暂不可尝试 ∧ 非关闭态（判死档 / 已取证 build 瞬时断链）；
 * - checking：开启、可尝试、尚无证据——连接后自动检查（设计四态外的合法中间态）；
 * - unknown：DTO 取不到 / 形状未知——不猜。
 */
export function codexAdaptUiState(
  s: CodexAdaptStatus | null,
  explicit: boolean | null,
  loadFailed: boolean
): CodexAdaptUiState {
  if (loadFailed || s === null) return "unknown";
  if (explicit === false || s.autoAdaptExplicit === false) return "off";
  if (s.probeStatus.state === "probing") return "probing";
  if (s.evidenced.state || s.evidenced.send || s.evidenced.schema) return "adapted";
  if (!s.attemptable.send) return "error";
  return "checking";
}

const STATE_TONE: Record<CodexAdaptUiState, string> = {
  off: "bg-secondary text-secondary-foreground",
  probing: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  adapted: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  error: "bg-rose-500/15 text-rose-600 dark:text-rose-400",
  checking: "bg-blue-500/15 text-blue-600 dark:text-blue-400",
  unknown: "bg-secondary text-secondary-foreground",
};

const STATE_LABEL_KEY: Record<CodexAdaptUiState, string> = {
  off: "settings.remote.codexAdaptStateOff",
  probing: "settings.remote.codexAdaptStateProbing",
  adapted: "settings.remote.codexAdaptStateAdapted",
  error: "settings.remote.codexAdaptStateError",
  checking: "settings.remote.codexAdaptStateChecking",
  unknown: "settings.remote.codexAdaptStateUnknown",
};

const STATE_HINT_KEY: Record<CodexAdaptUiState, string> = {
  off: "settings.remote.codexAdaptStateOffHint",
  probing: "settings.remote.codexAdaptStateProbingHint",
  adapted: "settings.remote.codexAdaptStateAdaptedHint",
  error: "settings.remote.codexAdaptStateErrorHint",
  checking: "settings.remote.codexAdaptStateCheckingHint",
  unknown: "settings.remote.codexAdaptStateUnknownHint",
};

interface CodexAdaptSectionProps {
  /** 开关生效值（remote_status.codexAutoAdaptEnabled；缺键按缺省开） */
  enabled: boolean;
  /** 是否显式关（remote_status.codexAutoAdaptExplicit === false） */
  explicit: boolean | null;
  /** remote_status 是否已加载（未载前开关禁用，不猜初值） */
  loaded: boolean;
  /** 开关在途互斥（与分区其他开关共用 busy 闸） */
  busy: boolean;
  /** 开关切换（RemoteSection 落盘 + 刷新 status） */
  onToggle: (v: boolean) => void;
}

export function CodexAdaptSection({
  enabled,
  explicit,
  loaded,
  busy,
  onToggle,
}: CodexAdaptSectionProps) {
  const { t } = useAppTranslation();
  // 首次轻提示：null = 记录键读取中（不闪现）
  const [hintSeen, setHintSeen] = useState<boolean | null>(null);
  // 适配状态 DTO（null = 未达 / 失败——按 unknown 呈现，不猜）
  const [adapt, setAdapt] = useState<CodexAdaptStatus | null>(null);
  const [adaptFailed, setAdaptFailed] = useState(false);
  const [detailOpen, setDetailOpen] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const statusRequest = useRef(0);

  useEffect(() => {
    void (async () => setHintSeen((await getSetting(CODEX_ADAPT_HINT_KEY)) === "true"))();
  }, []);

  const loadAdapt = useCallback(async () => {
    const request = ++statusRequest.current;
    setRefreshing(true);
    try {
      const next = await codexAdaptStatus();
      if (request !== statusRequest.current) return;
      setAdapt(next);
      setAdaptFailed(false);
    } catch {
      if (request !== statusRequest.current) return;
      setAdapt(null);
      setAdaptFailed(true);
    } finally {
      if (request === statusRequest.current) setRefreshing(false);
    }
  }, []);

  // 挂载 + 开关翻转后重读（关闭/重开即时反映到状态行）
  useEffect(() => {
    void loadAdapt();
  }, [loadAdapt, enabled]);

  const dismissHint = useCallback(async () => {
    try {
      await setSetting(CODEX_ADAPT_HINT_KEY, "true");
      setHintSeen(true);
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  }, [t]);

  const state = codexAdaptUiState(adapt, explicit, adaptFailed);
  const schema = adapt?.schemaEvidence ?? null;

  // 报告入口（R5 前身）：安全引用 only；未部署 → 明确不可用 + 复制摘要
  const [reportUnavailable, setReportUnavailable] = useState(false);
  const submitReport = useCallback(() => {
    const ref = {
      tool: "codex",
      surface: "settings_adapt",
      sessionId: null,
      actionId: null,
      errorCode: adapt?.lastFailure?.errorCode ?? null,
      adaptSummary: adaptSummaryOf(adapt),
    };
    if (!reportIssue(ref)) {
      setReportUnavailable(true); // 诊断服务未开启：如实呈现，不伪造成功
      return;
    }
    setReportUnavailable(false);
  }, [adapt]);

  const copySummary = useCallback(async () => {
    const text = reportIssueSummary({
      tool: "codex",
      surface: "settings_adapt",
      sessionId: null,
      actionId: null,
      errorCode: adapt?.lastFailure?.errorCode ?? null,
      adaptSummary: adaptSummaryOf(adapt),
    });
    try {
      await navigator.clipboard.writeText(text);
      toast.success(t("reportIssue.copied"));
    } catch {
      toast.error(t("settings.remote.copyFailed"));
    }
  }, [adapt, t]);

  return (
    <>
      <div className={`text-muted-foreground mt-4 ${SETTINGS_REMOTE_GROUP_LABEL} font-semibold`}>
        {t("settings.remote.groupCodexAdapt")}
      </div>
      {/* 首次轻提示（一次语义）：设计 §3.4 逐字口径，不宣称已解锁全部能力 */}
      {hintSeen === false && (
        <div
          data-testid="codex-adapt-hint"
          className="mt-1 flex items-start gap-3 rounded-lg bg-blue-500/10 px-2.5 py-1.5 text-xs text-blue-700 dark:text-blue-300"
        >
          <p className="flex-1">{t("settings.remote.codexAdaptHintBody")}</p>
          <Button
            size="sm"
            variant="outline"
            className="h-6 px-2 text-xs"
            data-testid="codex-adapt-hint-dismiss"
            onClick={() => void dismissHint()}
          >
            {t("settings.remote.codexAdaptHintDismiss")}
          </Button>
        </div>
      )}
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.codexAdaptTitle")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.codexAdaptHint")}
          </p>
        </div>
        <Switch
          data-testid="codex-adapt-switch"
          aria-label={t("settings.remote.codexAdaptTitle")}
          checked={enabled}
          disabled={busy || !loaded}
          onCheckedChange={(v) => onToggle(v)}
        />
      </div>
      {/* 状态行：四态徽标 + 一句话说明 + 详情开关（始终可见，详情点击展开） */}
      <div className="flex flex-wrap items-center gap-2">
        <span
          data-testid="codex-adapt-state"
          data-state={state}
          className={cn(
            "inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium",
            STATE_TONE[state]
          )}
        >
          {t(STATE_LABEL_KEY[state])}
        </span>
        <span className="text-muted-foreground text-xs">{t(STATE_HINT_KEY[state])}</span>
        <Button
          variant="outline"
          size="sm"
          className="h-6 px-2 text-xs"
          data-testid="codex-adapt-detail-toggle"
          aria-expanded={detailOpen}
          onClick={() => setDetailOpen((v) => !v)}
        >
          {detailOpen
            ? t("settings.remote.codexAdaptDetailHide")
            : t("settings.remote.codexAdaptDetailShow")}
        </Button>
      </div>
      {detailOpen && (
        <div
          data-testid="codex-adapt-detail"
          className="bg-muted/30 mt-2 space-y-1.5 rounded-lg border p-3 text-xs"
        >
          <div className="flex flex-wrap gap-1.5">
            <span className="text-muted-foreground">
              {t("settings.remote.codexAdaptDetailVersion")}
            </span>
            <span className="font-mono break-all">
              {adapt === null
                ? "—"
                : `${adapt.platform}${adapt.desktopBuild ? ` · ${adapt.desktopBuild}` : ""}`}
            </span>
          </div>
          <div className="flex flex-wrap gap-1.5">
            <span className="text-muted-foreground">
              {t("settings.remote.codexAdaptDetailFingerprint")}
            </span>
            <span className="font-mono break-all">
              {adapt?.probeKey ?? t("settings.remote.codexAdaptFingerprintNone")}
            </span>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="text-muted-foreground">
              {t("settings.remote.codexAdaptDetailEvidence")}
            </span>
            {adapt !== null &&
            (adapt.evidenced.state || adapt.evidenced.send || schema !== null) ? (
              <>
                {adapt.evidenced.state && (
                  <span className="rounded bg-emerald-500/15 px-1.5 py-0.5 text-emerald-600 dark:text-emerald-400">
                    {t("settings.remote.codexAdaptEvidenceState")}
                  </span>
                )}
                {adapt.evidenced.send && (
                  <span className="rounded bg-emerald-500/15 px-1.5 py-0.5 text-emerald-600 dark:text-emerald-400">
                    {t("settings.remote.codexAdaptEvidenceSend")}
                  </span>
                )}
                {schema !== null && (
                  <span className="rounded bg-emerald-500/15 px-1.5 py-0.5 text-emerald-600 dark:text-emerald-400">
                    {t("settings.remote.codexAdaptEvidenceSchema", {
                      n: schema.commandDecisions.length + schema.fileDecisions.length,
                    })}
                  </span>
                )}
              </>
            ) : (
              <span className="text-muted-foreground">
                {t("settings.remote.codexAdaptEvidenceNone")}
              </span>
            )}
          </div>
          <div className="flex flex-wrap gap-1.5">
            <span className="text-muted-foreground">
              {t("settings.remote.codexAdaptDetailConnection")}
            </span>
            <span>
              {adapt === null
                ? "—"
                : `${
                    adapt.attemptable.send
                      ? t("settings.remote.codexAdaptAttemptableSendYes")
                      : t("settings.remote.codexAdaptAttemptableSendNo")
                  } · ${
                    adapt.connectionEpoch > 0
                      ? t("settings.remote.codexAdaptConnected", { n: adapt.connectionEpoch })
                      : t("settings.remote.codexAdaptDisconnected")
                  }`}
            </span>
          </div>
          <div className="flex flex-wrap gap-1.5">
            <span className="text-muted-foreground">
              {t("settings.remote.codexAdaptDetailLastFailure")}
            </span>
            <span className="break-all">
              {adapt?.lastFailure
                ? `${adapt.lastFailure.capability} · ${adapt.lastFailure.state}${
                    adapt.lastFailure.errorCode ? ` · ${adapt.lastFailure.errorCode}` : ""
                  }${
                    adapt.lastFailure.observedAt
                      ? ` · ${new Date(adapt.lastFailure.observedAt).toLocaleString()}`
                      : ""
                  }`
                : t("settings.remote.codexAdaptLastFailureNone")}
            </span>
          </div>
          {/* 手动刷新（只读；终审裁定改名「刷新状态」——原「重新检查」暗示可触发
              重探，实际只刷新快照）：刷新当前适配状态快照——不含测试发送或审批；
              重新探测在下次连接时自动进行 */}
          <div className="flex flex-wrap items-center gap-2 pt-1">
            <Button
              variant="outline"
              size="sm"
              className="h-6 px-2 text-xs"
              data-testid="codex-adapt-refresh"
              disabled={refreshing}
              onClick={() => void loadAdapt()}
            >
              {t("settings.remote.codexAdaptRefresh")}
            </Button>
            <span className="text-muted-foreground">
              {t("settings.remote.codexAdaptRefreshHint")}
            </span>
          </div>
          {/* 排障入口（R5 前身）：统一报告事件 + 诊断服务未部署的如实出口 */}
          <div className="flex flex-wrap items-center gap-2 pt-1">
            <Button
              variant="outline"
              size="sm"
              className="h-6 px-2 text-xs"
              data-testid="codex-adapt-report"
              onClick={submitReport}
            >
              {t("reportIssue.button")}
            </Button>
            {reportUnavailable && (
              <span data-testid="codex-adapt-report-unavailable" className="text-muted-foreground">
                {t("reportIssue.unavailable")}
                <Button
                  variant="outline"
                  size="sm"
                  className="ml-2 h-6 px-2 text-xs"
                  data-testid="codex-adapt-report-copy"
                  onClick={() => void copySummary()}
                >
                  {t("reportIssue.copySummary")}
                </Button>
              </span>
            )}
          </div>
        </div>
      )}
    </>
  );
}

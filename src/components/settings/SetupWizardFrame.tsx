import { useEffect, useRef, useState, type ReactNode } from "react";
import { ChevronDown, ChevronUp } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { getSetting, setSetting } from "@/lib/api/settings";
import { toast } from "sonner";
import { ChannelQr } from "@/components/settings/ChannelQr";
import { ProcessHint } from "@/components/settings/ProcessHint";
import { SETTINGS_FIELD } from "@/components/settings/typography";

type Props = {
  storageKey: string;
  configKey: string | null;
  complete: boolean;
  address: string | null;
  pin?: string;
  offline?: boolean;
  issueText?: string | null;
  statusText?: string | null;
  issue: boolean;
  /** 通道在开、服务在跑、只差校验证据——自动复验在途，把「过程」讲给用户听 */
  reverifyPending?: boolean;
  children: ReactNode;
};

/** 两类远程向导共用成功摘要、可展开诊断和成功配置记忆。 */
export function SetupWizardFrame({
  storageKey,
  configKey,
  complete,
  address,
  pin = "",
  offline = false,
  issueText,
  statusText,
  issue,
  reverifyPending = false,
  children,
}: Props) {
  const { t } = useAppTranslation();
  const [savedKey, setSavedKey] = useState<string | null>(null);
  const [savedAddress, setSavedAddress] = useState<string | null>(null);
  const [historyLoaded, setHistoryLoaded] = useState(false);
  const [manualChoice, setManualChoice] = useState<{
    key: string | null;
    expanded: boolean;
  } | null>(null);
  const notified = useRef<string | null>(null);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    setHistoryLoaded(false);
    void getSetting(storageKey)
      .then((value) => {
        if (!alive) return;
        let record: { configKey: string; address: string | null } | null = null;
        if (value) {
          try {
            const parsed = JSON.parse(value) as { configKey?: unknown; address?: unknown };
            if (typeof parsed.configKey === "string") {
              record = {
                configKey: parsed.configKey,
                address: typeof parsed.address === "string" ? parsed.address : null,
              };
            }
          } catch {
            record = { configKey: value, address: null };
          }
        }
        setSavedKey(record?.configKey ?? null);
        setSavedAddress(record?.address ?? null);
        setHistoryLoaded(true);
      })
      .catch(() => {
        if (!alive) return;
        setSavedKey(null);
        setSavedAddress(null);
        setHistoryLoaded(true);
      });
    return () => {
      alive = false;
    };
  }, [storageKey]);

  useEffect(() => {
    if (!historyLoaded || !complete || !configKey) return;
    if (savedKey !== configKey || savedAddress !== address) {
      setSavedKey(configKey);
      setSavedAddress(address);
      void setSetting(storageKey, JSON.stringify({ configKey, address })).catch(() => {});
    }
    if (savedKey !== configKey) {
      if (notified.current !== configKey) {
        notified.current = configKey;
        toast.success(t("settings.remote.setupCompleteNotice"));
        requestAnimationFrame(() =>
          root.current?.scrollIntoView?.({ behavior: "smooth", block: "nearest" })
        );
      }
    }
  }, [
    address,
    complete,
    configKey,
    historyLoaded,
    manualChoice,
    savedAddress,
    savedKey,
    storageKey,
    t,
  ]);

  // 配置标识尚在探测时只恢复历史展示；它不代表当前地址已验证。
  const currentHistory = Boolean(savedKey && (!configKey || savedKey === configKey));
  const displayKey = configKey ?? savedKey;
  const detailsOpen =
    manualChoice?.key === displayKey ? manualChoice.expanded : !(complete || currentHistory);
  const summaryText = issue
    ? t("settings.remote.setupNeedsAttention")
    : complete
      ? t("settings.remote.setupAvailable")
      : statusText
        ? statusText
        : offline
          ? t("settings.remote.setupConfiguredOff")
          : t("settings.remote.setupNotVerified");
  const summaryAddress = complete ? address : currentHistory ? savedAddress : null;
  const copyAddress = async () => {
    if (!summaryAddress) return;
    try {
      await navigator.clipboard.writeText(summaryAddress);
      toast.success(t("settings.remote.copied"));
    } catch {
      toast.error(t("settings.remote.copyFailed"));
    }
  };

  return (
    <section
      ref={root}
      data-testid="setup-wizard-frame"
      data-complete={complete ? "true" : "false"}
    >
      {complete || currentHistory ? (
        <div
          className={`mb-2 flex flex-wrap items-center justify-between gap-2 rounded-lg border px-3 py-2 ${complete ? "border-emerald-500/30 bg-emerald-500/5" : "border-amber-500/30 bg-amber-500/5"}`}
        >
          <div className="min-w-0">
            <p className={SETTINGS_FIELD}>{summaryText}</p>
            {issueText && (
              <p data-testid="setup-issue" className="text-xs text-amber-700">
                {issueText}
              </p>
            )}
            {summaryAddress && (
              <code
                className={`text-xs break-all ${complete ? "text-muted-foreground" : "text-muted-foreground/70"}`}
              >
                {summaryAddress}
              </code>
            )}
            <p className="text-muted-foreground text-xs">
              {t(
                complete
                  ? "settings.remote.setupAddressLocation"
                  : "settings.remote.setupAddressUnavailable"
              )}
            </p>
            {/* 过程层（2026-10-22 用户验收反馈）：状态层只说「尚未验证」，不解释
              「系统正在自动复验、等多久、超时才算异常」——用户无从区分正常等待与故障。
              信号由调用方推导（enabled∧running∧unverified），此处只负责呈现。 */}
            {!complete && !issue && reverifyPending && (
              <ProcessHint testId="setup-reverify-hint">
                {t("settings.remote.setupReverifyHint")}
              </ProcessHint>
            )}
            {complete && address && (
              <div className="mt-2">
                <ChannelQr
                  url={pin ? `${address}#pin=${pin}` : address}
                  caption={`${t("settings.remote.qrTitle")} · ${t("settings.remote.qrAutoPin")}`}
                />
              </div>
            )}
          </div>
          {summaryAddress && (
            <div className="flex items-center gap-1">
              <Button variant="outline" size="sm" onClick={() => void copyAddress()}>
                {t("settings.remote.copy")}
              </Button>
              {complete && (
                <a
                  className="inline-flex h-8 items-center rounded-md border px-3 text-sm"
                  href={address ?? undefined}
                  target="_blank"
                  rel="noreferrer noopener"
                >
                  {t("settings.remote.setupOpen")}
                </a>
              )}
            </div>
          )}
          <Button
            variant="ghost"
            size="sm"
            aria-expanded={detailsOpen}
            onClick={() => {
              setManualChoice({ key: displayKey, expanded: !detailsOpen });
            }}
          >
            {detailsOpen ? (
              <ChevronUp className="mr-1 h-4 w-4" />
            ) : (
              <ChevronDown className="mr-1 h-4 w-4" />
            )}
            {t(
              detailsOpen
                ? "settings.remote.setupHideDetails"
                : issue
                  ? "settings.remote.setupViewDiagnosis"
                  : "settings.remote.setupShowDetails"
            )}
          </Button>
        </div>
      ) : null}
      {detailsOpen && children}
    </section>
  );
}

import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Check } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { SetupWizardFrame } from "@/components/settings/SetupWizardFrame";
import { ProcessHint } from "@/components/settings/ProcessHint";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { formatInvokeError } from "@/lib/invokeError";
import {
  reachChecking,
  remoteNamedProbe,
  remoteNamedSetAddress,
  remoteNamedVerify,
  toggleChannel,
  type NamedWizardProbe,
} from "@/lib/api/remote";
import { toast } from "sonner";
import { SETTINGS_FIELD } from "@/components/settings/typography";

export function NamedDomainWizard({
  hasToken,
  refreshKey,
  pin = "",
  tokenEditor,
  remoteEnabled = true,
}: {
  hasToken: boolean;
  refreshKey: string;
  pin?: string;
  tokenEditor?: ReactNode;
  remoteEnabled?: boolean;
}) {
  const { t } = useAppTranslation();
  const [probe, setProbe] = useState<NamedWizardProbe | null>(null);
  const [address, setAddress] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorStep, setErrorStep] = useState<string | null>(null);
  const [now, setNow] = useState(0);
  const addressDirty = useRef(false);
  const lifecycle = useRef(0);
  const latestProbe = useRef(0);

  const load = useCallback(async () => {
    const request = ++latestProbe.current;
    const generation = lifecycle.current;
    try {
      const next = await remoteNamedProbe();
      if (request !== latestProbe.current || generation !== lifecycle.current) return;
      setProbe(next);
      // 时钟在探测落地那一刻播种（2026-10-22）：等待元数据首帧即真值——渲染期读
      // Date.now() 过不了 purity 规则，播种晚一拍又会让首帧闪「0 分钟」
      if (next.waiting) setNow(Date.now());
      if (!addressDirty.current) setAddress(next.address ?? "");
      setError(null);
      setErrorStep(null);
    } catch (e) {
      if (request !== latestProbe.current || generation !== lifecycle.current) return;
      setError(formatInvokeError(e, t));
      setErrorStep("probe");
    }
  }, [t]);

  useEffect(() => {
    lifecycle.current += 1;
    setProbe(null);
    setError(null);
    setErrorStep(null);
    void load();
    return () => {
      lifecycle.current += 1;
      latestProbe.current += 1;
    };
  }, [load, refreshKey, hasToken]);
  useEffect(() => {
    if (!probe?.waiting) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [probe?.waiting]);

  const act = async (action: () => Promise<NamedWizardProbe>, step: string) => {
    if (busy) return;
    const generation = lifecycle.current;
    latestProbe.current += 1;
    setBusy(true);
    setError(null);
    try {
      const next = await action();
      if (generation !== lifecycle.current) return;
      setProbe(next);
      if (next.waiting) setNow(Date.now());
      if (step === "bind") {
        addressDirty.current = false;
        setAddress(next.address ?? "");
      }
      setErrorStep(null);
    } catch (e) {
      if (generation !== lifecycle.current) return;
      const message = formatInvokeError(e, t);
      await load();
      if (generation !== lifecycle.current) return;
      setError(message);
      setErrorStep(step);
    } finally {
      setBusy(false);
    }
  };
  const saveAddress = async () => {
    await act(() => remoteNamedSetAddress(address.trim()), "bind");
  };
  const connect = async () => {
    if (busy) return;
    const generation = lifecycle.current;
    latestProbe.current += 1;
    setBusy(true);
    setError(null);
    try {
      await toggleChannel("named", true);
      if (generation !== lifecycle.current) return;
      await load();
    } catch (e) {
      if (generation !== lifecycle.current) return;
      setError(formatInvokeError(e, t));
      setErrorStep("connect");
    } finally {
      setBusy(false);
    }
  };
  const copyOrigin = async () => {
    if (!probe?.originUrl) return;
    try {
      await navigator.clipboard.writeText(probe.originUrl);
      toast.success(t("settings.remote.copied"));
    } catch {
      toast.error(t("settings.remote.copyFailed"));
    }
  };
  const copySuggestedSubdomain = async () => {
    try {
      await navigator.clipboard.writeText("tuvis");
      toast.success(t("settings.remote.copied"));
    } catch {
      toast.error(t("settings.remote.copyFailed"));
    }
  };
  const configured = Boolean(probe?.address);
  const reachReason = probe?.reach.state === "failed" ? probe.reach.reason : null;
  const authFailed = Boolean(reachReason?.includes("REMOTE_NAMED_AUTH_FAIL"));
  const verified =
    probe?.reach.state === "verified" && probe.connected && probe.enabled && probe.running;
  const configuredOff = Boolean(probe && (!probe.enabled || !remoteEnabled));
  const complete = Boolean(remoteEnabled && probe?.configKey && hasToken && verified && configured);
  const issue = Boolean(error || (!configuredOff && probe?.reach.state === "failed"));
  // now 由探测落地/秒级 tick 播种（播种点见 load/act）；等待元数据首帧渲染时必有真值
  const elapsed = probe?.waiting
    ? Math.floor(Math.max(0, now - probe.waiting.startedAtMs) / 60000)
    : 0;
  const steps = [
    {
      id: "create",
      done: Boolean(probe?.connected),
      label: t("settings.remote.namedWizard.createTitle"),
      detail: t(
        hasToken && probe?.hasToken && !probe.connected
          ? "settings.remote.namedWizard.createTokenProvided"
          : "settings.remote.namedWizard.createHint"
      ),
    },
    {
      id: "connect",
      done: Boolean(hasToken && probe?.connected),
      label: t("settings.remote.namedWizard.connectTitle"),
      detail: t("settings.remote.namedWizard.connectHint"),
    },
    {
      id: "bind",
      done: Boolean(verified),
      label: t("settings.remote.namedWizard.bindTitle"),
      detail: t("settings.remote.namedWizard.bindHint"),
    },
    {
      id: "verify",
      done: verified,
      label: t("settings.remote.namedWizard.verifyTitle"),
      detail: t("settings.remote.namedWizard.verifyHint"),
    },
  ];

  // 「自动复验在途」判据见 reachChecking（shared，与 Tailscale 同口径）
  const reachCheckingNow = probe ? reachChecking(probe.reach) : false;

  return (
    <SetupWizardFrame
      storageKey="remote.setup.named"
      configKey={probe?.configKey ?? null}
      complete={complete}
      address={verified ? probe?.address : null}
      pin={pin}
      offline={configuredOff}
      reverifyPending={Boolean(
        remoteEnabled && probe?.enabled && probe.running && probe.connected && reachCheckingNow
      )}
      issue={issue}
      issueText={
        error ?? (configuredOff || !reachReason ? null : formatInvokeError(reachReason, t))
      }
    >
      <div data-testid="named-wizard" className="mt-3 space-y-3">
        <ol aria-label={t("settings.remote.namedWizard.timelineLabel")} className="space-y-2">
          {steps.map((step, index) => (
            <li
              key={step.id}
              data-step={step.id}
              data-done={step.done ? "true" : "false"}
              className="grid grid-cols-[minmax(0,1fr)_minmax(180px,300px)] items-start gap-3 rounded-lg border p-3"
            >
              <div className="flex min-w-0 gap-3">
                <span className={step.done ? "text-emerald-600" : "text-muted-foreground"}>
                  {step.done ? <Check className="h-5 w-5" /> : index + 1}
                </span>
                <div className="min-w-0">
                  <p className={SETTINGS_FIELD}>{step.label}</p>
                  <p className="text-muted-foreground text-xs">{step.detail}</p>
                  {step.id === "bind" && (
                    <p className="text-muted-foreground mt-1 text-xs break-all">
                      {t("settings.remote.namedWizard.originUrl")}:{" "}
                      <code>{probe?.originUrl ?? "—"}</code>
                    </p>
                  )}
                  {step.id === "bind" && (
                    <p className="text-muted-foreground mt-1 text-xs">
                      {t("settings.remote.namedWizard.bindRouteHint")}
                    </p>
                  )}
                  {step.id === "connect" && probe?.connected && (
                    <p className="mt-1 text-xs text-emerald-600">
                      {t("settings.remote.namedWizard.connected")}
                    </p>
                  )}
                  {step.id === "verify" && reachReason && (
                    <p className="mt-1 text-xs text-amber-600">
                      {formatInvokeError(reachReason, t)}
                    </p>
                  )}
                  {step.id === "connect" && authFailed && (
                    <p role="alert" className="mt-1 text-xs text-amber-600">
                      {t("settings.remote.namedWizard.authFailHint")}
                    </p>
                  )}
                  {error && errorStep === step.id && (
                    <p role="alert" className="mt-1 text-xs text-rose-600">
                      {error}
                    </p>
                  )}
                  {error && errorStep === "probe" && step.id === "verify" && (
                    <p role="alert" className="mt-1 text-xs text-rose-600">
                      {error}
                    </p>
                  )}
                </div>
              </div>
              <div className="flex min-w-0 flex-col items-stretch gap-2">
                {step.id === "create" && (
                  <a
                    className="text-right text-xs text-blue-600 underline"
                    href="https://dash.cloudflare.com/"
                    target="_blank"
                    rel="noreferrer noopener"
                  >
                    {t("settings.remote.namedWizard.openCloudflare")}
                  </a>
                )}
                {step.id === "connect" && (
                  <>
                    {tokenEditor}
                    {!probe?.connected &&
                      (hasToken ? (
                        <Button size="sm" disabled={busy} onClick={() => void connect()}>
                          {t("settings.remote.namedWizard.connect")}
                        </Button>
                      ) : (
                        <span className="text-right text-xs text-amber-600">
                          {t("settings.remote.namedWizard.saveTokenFirst")}
                        </span>
                      ))}
                  </>
                )}
                {step.id === "bind" && (
                  <>
                    <div className="flex flex-wrap items-center justify-end gap-2">
                      <span className="text-muted-foreground text-xs">
                        {t("settings.remote.namedWizard.suggestedSubdomain")}
                      </span>
                      <code>tuvis</code>
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => void copySuggestedSubdomain()}
                      >
                        {t("settings.remote.namedWizard.copySuggested")}
                      </Button>
                    </div>
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={!probe?.originUrl}
                      onClick={() => void copyOrigin()}
                    >
                      {t("settings.remote.namedWizard.copyOrigin")}
                    </Button>
                    <Input
                      aria-label={t("settings.remote.namedWizard.publicAddress")}
                      value={address}
                      onChange={(e) => {
                        addressDirty.current = true;
                        setAddress(e.target.value);
                      }}
                      placeholder="https://remote.example.com/m"
                    />
                    <Button
                      size="sm"
                      disabled={busy || !probe?.connected || !address.trim()}
                      onClick={() => void saveAddress()}
                    >
                      {t("settings.remote.namedWizard.saveAddress")}
                    </Button>
                  </>
                )}
                {step.id === "verify" && (
                  <>
                    {(probe?.reach.state === "record_pending" ||
                      probe?.reach.state === "verifying") &&
                    probe.waiting ? (
                      <ProcessHint testId="named-waiting">
                        {t("settings.remote.namedWizard.waitingDns", {
                          elapsed,
                          expected:
                            probe.waiting.expectedSeconds === null
                              ? "—"
                              : Math.ceil(probe.waiting.expectedSeconds / 60),
                        })}
                        {probe.reach.state === "verifying" &&
                          ` ${t("settings.remote.namedWizard.verifyingNow")}`}
                      </ProcessHint>
                    ) : null}
                    {probe?.reach.state === "verifying" && !probe.waiting && (
                      <ProcessHint testId="named-verifying">
                        {t("settings.remote.namedWizard.verifyingNow")}
                      </ProcessHint>
                    )}
                    {!verified &&
                      probe?.reach.state !== "record_pending" &&
                      probe?.reach.state !== "verifying" &&
                      !probe?.waiting && (
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={busy || !probe?.connected || !configured}
                          onClick={() => void act(remoteNamedVerify, "verify")}
                        >
                          {t("settings.remote.namedWizard.verify")}
                        </Button>
                      )}
                  </>
                )}
              </div>
            </li>
          ))}
        </ol>
        {error && errorStep === "probe" && !probe && (
          <p role="alert" className="text-xs text-rose-600">
            {error}
          </p>
        )}
        {probe?.address && !verified && (
          <p className="text-muted-foreground text-xs break-all">
            {t("settings.remote.namedWizard.addressNotVerified", { address: probe.address })}
          </p>
        )}
      </div>
    </SetupWizardFrame>
  );
}

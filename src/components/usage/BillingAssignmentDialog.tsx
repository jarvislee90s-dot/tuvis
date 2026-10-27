// 记录页「绑定计费归属」Dialog（Task 13；2026-10-21 起仅含糊行保留入口）。
//
// Props 严格为 `{open,onOpenChange,identities,range,t}`；billing 配置由本组件内部查询。
// 流程：选候选身份 → 选/新建渠道 → 匹配方式与生效时间 → **预览** → **确认保存**。
// 预览与保存是两个独立动作；候选/渠道/规则/日期/mixed 任一变化都会清空旧 preview。
import { useEffect, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { usageBillingCommit, usageBillingPreview } from "@/lib/api/usageV2";
import { USAGE_KEY, useUsageBillingQuery } from "@/lib/query/queries/usageV2";
import { EM_DASH, fmtDateTime, fmtTokens, usageAgentLabel } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import type { TFn } from "@/lib/usage/range";
import { usageErrMsg } from "@/components/usage/usageErrors";
import type { UsageRange } from "@/types/usage";
import type { BillingConfig, BillingPreview, BillingProfile, IdentityUsage } from "@/types/usageV2";

export interface BillingAssignmentDialogProps {
  open: boolean;
  onOpenChange(open: boolean): void;
  /** 点击行对应的候选身份（同名碰撞时包含全部候选，已由调用方按完整身份筛选） */
  identities: IdentityUsage[];
  range: UsageRange;
  t: TFn;
  /**
   * active 门控（第三/四轮评审登记的 follow-up）：四页宿主的隐藏页期间停发 billing
   * 读查询（含 focus refetch）。默认 true，旧调用者行为不变。
   */
  active?: boolean;
}

/** 完整身份显示名：provider 非空 `${provider} / ${modelId}`，否则 modelId */
export function identityDisplayName(identity: IdentityUsage): string {
  const provider = identity.identity.observedProvider;
  return provider ? `${provider} / ${identity.identity.modelId}` : identity.identity.modelId;
}

function startOfDayMs(date: string): number | null {
  if (!date) return null;
  const [y, m, d] = date.split("-").map(Number);
  if (!y || !m || !d) return null;
  return new Date(y, m - 1, d).getTime();
}

export function BillingAssignmentDialog({
  open,
  onOpenChange,
  identities,
  range,
  t,
  active = true,
}: BillingAssignmentDialogProps) {
  const queryClient = useQueryClient();
  const lang = getLang();
  // billing 读查询以「对话框打开 + 页面 active」为门控（页面隐藏期间停发，含 focus refetch）
  const billing = useUsageBillingQuery({ enabled: open && active });
  const [selectedIndex, setSelectedIndex] = useState<number | null>(null);
  const [profileChoice, setProfileChoice] = useState("");
  /** 用户是否已手动选过渠道：手选后自动填充永不覆盖（即便 billing 随后台 refetch 变化） */
  const [profileTouched, setProfileTouched] = useState(false);
  const [newName, setNewName] = useState("");
  const [match, setMatch] = useState<"exact" | "prefix">("exact");
  const [mixed, setMixed] = useState(false);
  const [historical, setHistorical] = useState(false);
  const [historicalDate, setHistoricalDate] = useState("");
  const [preview, setPreview] = useState<BillingPreview | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [newProfileId] = useState(() => crypto.randomUUID());

  const profiles = billing.data?.profiles ?? [];
  const activeProfiles = profiles.filter((p) => !p.archived);

  // 打开时重置选择/草稿；候选只有一条时自动选中（多条必须显式选择，不静默选第一项）。
  // 依赖只留 open：打开期间 identities 引用随后台 refetch 变化不得清空已选候选与预览；
  // 「换行时重置」由调用方卸载重挂保证（AssignmentLoader 随 assign 置空整体卸载）。
  useEffect(() => {
    if (!open) return;
    setSelectedIndex(identities.length === 1 ? 0 : null);
    setProfileChoice("");
    setProfileTouched(false);
    setPreview(null);
    setError(null);
    setNowMs(Date.now());
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 意图=仅随 open 翻转重置
  }, [open]);

  // 默认渠道：billing 就绪（!isPending）后才取第一个非归档渠道；没有则「新建渠道」。
  // pending 期间 activeProfiles 为空，绝不先落 "__new__"，否则数据到达后无法自动纠正；
  // 用户一旦手选（profileTouched）就不再自动填充。
  useEffect(() => {
    if (!open || profileTouched || billing.isPending) return;
    const first = activeProfiles[0];
    setProfileChoice(first ? first.id : "__new__");
  }, [open, profileTouched, billing.isPending, activeProfiles]);

  // 任何候选/规则变化立即清空旧 preview（保存按钮随之禁用）
  useEffect(() => {
    setPreview(null);
  }, [selectedIndex, profileChoice, newName, match, mixed, historical, historicalDate]);

  const selected = selectedIndex === null ? null : (identities[selectedIndex] ?? null);
  const targetProfile: BillingProfile | null =
    profileChoice === "__new__" ? null : (profiles.find((p) => p.id === profileChoice) ?? null);

  const effectiveFrom = historical ? (startOfDayMs(historicalDate) ?? nowMs) : nowMs;

  const nameMissing = profileChoice === "__new__" && newName.trim() === "";
  const profileValid =
    profileChoice === "__new__" ? !nameMissing : targetProfile !== null && !targetProfile.archived;
  const canBuild = selected !== null && profileValid && billing.data !== null;

  const candidate: BillingConfig | null = useMemo(() => {
    if (!canBuild || !billing.data || !selected) return null;
    const base = billing.data;
    const targetId = profileChoice === "__new__" ? newProfileId : profileChoice;
    const nextProfiles =
      profileChoice === "__new__"
        ? [
            ...base.profiles,
            {
              id: newProfileId,
              name: newName.trim(),
              kind: "unknown" as const,
              note: "",
              archived: false,
            },
          ]
        : base.profiles;
    return {
      ...base,
      profiles: nextProfiles,
      rules: [
        ...base.rules,
        {
          id: crypto.randomUUID(),
          // 复制完整 IdentityKey，不读 UI 截断文本
          identity: { ...selected.identity },
          match,
          billingProfileId: targetId,
          effectiveFrom,
          effectiveTo: null,
          mixed,
        },
      ],
    };
  }, [
    canBuild,
    billing.data,
    selected,
    profileChoice,
    newProfileId,
    newName,
    match,
    effectiveFrom,
    mixed,
  ]);

  const handlePreview = async () => {
    if (!candidate) return;
    setBusy(true);
    setError(null);
    try {
      setPreview(await usageBillingPreview(candidate, range));
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const handleSave = async () => {
    if (!preview) return;
    setBusy(true);
    setError(null);
    try {
      await usageBillingCommit({
        expectedRevision: preview.baseRevision,
        candidate: preview.candidate,
      });
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
      onOpenChange(false);
    } catch (e) {
      // 冲突保留草稿并重拉 billing（旧 revision 已失效）；旧 preview 同时作废，必须重新预览
      setError(e);
      setPreview(null);
      void billing.refetch();
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] max-w-2xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t("usage.billing.title")}</DialogTitle>
          <DialogDescription>{t("usage.billing.desc")}</DialogDescription>
        </DialogHeader>

        {billing.isPending ? (
          <p className="text-muted-foreground text-sm">{t("common.loading")}</p>
        ) : (
          <div className="space-y-4 text-sm">
            {/* 候选身份 */}
            <section className="space-y-1.5">
              <h3 className="text-muted-foreground text-xs font-medium">
                {t("usage.billing.candidates")}
              </h3>
              {identities.length === 0 ? (
                <p className="text-muted-foreground text-sm">{t("usage.empty")}</p>
              ) : (
                <div className="space-y-1">
                  {identities.map((id, i) => (
                    <button
                      key={`${id.identity.sourceId}:${id.identity.modelId}:${i}`}
                      type="button"
                      data-testid={`billing-candidate-${i}`}
                      aria-pressed={selectedIndex === i}
                      className={`flex w-full flex-col items-start gap-0.5 rounded-md border px-2 py-1.5 text-left ${
                        selectedIndex === i ? "bg-accent text-accent-foreground" : ""
                      }`}
                      onClick={() => setSelectedIndex(i)}
                    >
                      <span className="flex w-full items-center justify-between gap-2">
                        <span className="min-w-0 truncate">{identityDisplayName(id)}</span>
                        <span className="text-muted-foreground shrink-0 text-xs">
                          {usageAgentLabel(id.identity.sourceId)} ·{" "}
                          {fmtTokens(id.metrics.requestTotal + id.buckets.output, lang)}
                        </span>
                      </span>
                      {/* 每条候选都展示当前归属（不只是选中项） */}
                      <span className="text-muted-foreground text-[11px]">
                        {t("usage.billing.current")}:{" "}
                        {id.attribution.billingProfileId
                          ? (profiles.find((p) => p.id === id.attribution.billingProfileId)?.name ??
                            id.attribution.billingProfileId)
                          : t("usage.billing.unassigned")}{" "}
                        · {t(`usage.billing.attribution.${id.attribution.attributionKind}`)}
                      </span>
                    </button>
                  ))}
                </div>
              )}
            </section>

            {selected ? (
              <>
                {/* 只读身份 */}
                <section
                  data-testid="billing-readonly-identity"
                  className="grid gap-1 rounded-md border p-2 text-xs sm:grid-cols-2"
                >
                  <div>
                    <span className="text-muted-foreground">{t("usage.billing.source")}:</span>{" "}
                    {usageAgentLabel(selected.identity.sourceId)}
                  </div>
                  <div>
                    <span className="text-muted-foreground">{t("usage.billing.rawProvider")}:</span>{" "}
                    {selected.identity.observedProvider ?? EM_DASH}
                  </div>
                  <div className="sm:col-span-2">
                    <span className="text-muted-foreground">{t("usage.billing.model")}:</span>{" "}
                    <span className="font-mono">{selected.identity.modelId}</span>
                  </div>
                  <div className="sm:col-span-2">
                    <span className="text-muted-foreground">{t("usage.billing.current")}:</span>{" "}
                    {selected.attribution.billingProfileId
                      ? (profiles.find((p) => p.id === selected.attribution.billingProfileId)
                          ?.name ?? selected.attribution.billingProfileId)
                      : t("usage.billing.unassigned")}{" "}
                    · {t(`usage.billing.attribution.${selected.attribution.attributionKind}`)}
                  </div>
                </section>

                {/* 渠道选择/新建 */}
                <label className="flex items-center justify-between gap-2">
                  <span>{t("usage.billing.profile")}</span>
                  <select
                    data-testid="billing-profile-select"
                    className="bg-background h-8 max-w-[18rem] rounded-md border px-2 text-sm"
                    value={profileChoice}
                    onChange={(e) => {
                      setProfileTouched(true);
                      setProfileChoice(e.target.value);
                    }}
                  >
                    {profiles.map((p) => (
                      <option key={p.id} value={p.id} disabled={p.archived}>
                        {p.name}
                        {p.archived ? ` (${t("usage.billing.archived")})` : ""}
                      </option>
                    ))}
                    <option value="__new__">{t("usage.billing.newProfile")}</option>
                  </select>
                </label>

                {profileChoice === "__new__" ? (
                  <label className="flex items-center justify-between gap-2">
                    <span>{t("usage.billing.newProfileName")}</span>
                    <Input
                      data-testid="billing-new-profile-name"
                      className="h-8 max-w-[18rem]"
                      value={newName}
                      onChange={(e) => setNewName(e.target.value)}
                      placeholder={t("usage.billing.newProfilePlaceholder")}
                    />
                  </label>
                ) : null}

                <label className="flex items-center justify-between gap-2">
                  <span>{t("usage.billing.match")}</span>
                  <select
                    data-testid="billing-match-mode"
                    className="bg-background h-8 rounded-md border px-2 text-sm"
                    value={match}
                    onChange={(e) => setMatch(e.target.value as "exact" | "prefix")}
                  >
                    <option value="exact">{t("usage.billing.exact")}</option>
                    <option value="prefix">{t("usage.billing.prefix")}</option>
                  </select>
                </label>

                <div className="space-y-1">
                  <p className="text-muted-foreground text-xs">
                    {t("usage.billing.effectiveFrom", {
                      time: fmtDateTime(effectiveFrom),
                    })}
                  </p>
                  <label className="flex items-center gap-2 text-xs">
                    <input
                      type="checkbox"
                      data-testid="billing-historical"
                      checked={historical}
                      onChange={(e) => setHistorical(e.target.checked)}
                    />
                    {t("usage.billing.historical")}
                  </label>
                  {historical ? (
                    <>
                      <Input
                        type="date"
                        data-testid="billing-historical-date"
                        className="h-8 max-w-[12rem]"
                        value={historicalDate}
                        onChange={(e) => setHistoricalDate(e.target.value)}
                      />
                      <p className="text-muted-foreground text-xs">
                        {t("usage.billing.historicalHint")}
                      </p>
                    </>
                  ) : null}
                </div>

                <label className="flex items-start gap-2 text-xs">
                  <input
                    type="checkbox"
                    data-testid="billing-mixed"
                    checked={mixed}
                    onChange={(e) => setMixed(e.target.checked)}
                  />
                  {t("usage.billing.mixed")}
                </label>

                {nameMissing ? (
                  <p className="text-destructive text-xs">{t("usage.billing.nameRequired")}</p>
                ) : null}
              </>
            ) : identities.length > 0 ? (
              <p className="text-muted-foreground text-xs">{t("usage.billing.selectCandidate")}</p>
            ) : null}

            {error ? (
              <p data-testid="billing-error" className="text-destructive text-sm" role="alert">
                {usageErrMsg(error, t)}
              </p>
            ) : null}

            {preview ? (
              <section
                data-testid="billing-preview"
                className="space-y-1 rounded-md border p-2 text-xs"
              >
                <div>
                  {t("usage.billing.previewMatched", {
                    v: fmtTokens(preview.matchedTokens, lang),
                  })}
                </div>
                <div>
                  {t("usage.billing.previewAmbiguous", {
                    v: fmtTokens(preview.ambiguousTokens, lang),
                  })}
                </div>
                <div>
                  {t("usage.billing.previewBoundary", {
                    v: fmtTokens(preview.boundaryTokens, lang),
                  })}
                </div>
                <div>
                  {t("usage.billing.matchedIdentities")}:{" "}
                  {preview.matchedIdentities.map((id) => identityDisplayName(id)).join(", ") ||
                    EM_DASH}
                </div>
              </section>
            ) : (
              <p className="text-muted-foreground text-xs">{t("usage.billing.previewRequired")}</p>
            )}
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" size="sm" onClick={() => onOpenChange(false)}>
            {t("usage.billing.cancel")}
          </Button>
          <Button
            size="sm"
            variant="outline"
            data-testid="billing-preview-button"
            disabled={!canBuild || busy}
            onClick={() => void handlePreview()}
          >
            {t("usage.billing.preview")}
          </Button>
          <Button
            size="sm"
            data-testid="billing-save-button"
            disabled={!preview || busy}
            onClick={() => void handleSave()}
          >
            {t("usage.billing.save")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

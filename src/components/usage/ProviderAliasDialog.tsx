// 记录页「供应商别名」浮窗（Task 7）。
//
// 与 `BillingAssignmentDialog` **独立**：本浮窗只写 `usage_provider_alias` 表，不碰计费配置、
// 不改账本、不触发采集。候选身份由调用方（`AssignmentLoader`）按卡片作用域查询并做同名筛选；
// 多条同名候选时必须显式选择（与计费浮窗同一纪律，不静默选第一项）。
//
// 规则键（与后端 `ProviderAliasResolver` 一致）：
//  * 原始 provider 非空 → `kind="provider"`，作用于该原始串的所有工具/模型；
//  * 原始 provider 为空 → `kind="toolModel"`，只作用于该 `sourceId + modelId`（空串 = 未知模型）。
// 已有同键规则复用其 id 覆盖更新，否则 `crypto.randomUUID()` 新建。
import { useEffect, useState } from "react";
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
import {
  aliasModelLabel,
  aliasScopeOfIdentity,
  findAliasForIdentity,
  type AliasScope,
} from "@/components/usage/ProviderAliasFields";
import { identityDisplayName } from "@/components/usage/BillingAssignmentDialog";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { usageProviderAliasUpsert } from "@/lib/api/usageV2";
import { USAGE_KEY, useUsageIdentitiesQuery } from "@/lib/query/queries/usageV2";
import { EM_DASH, fmtTokens, usageAgentLabel } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import type { TFn } from "@/lib/usage/range";
import type { IdentityUsage, ProviderAliasRule } from "@/types/usageV2";

/** 规则的实际命中面（只统计近 30 天观测窗口内的规则命中项）。 */
interface AliasHitPreview {
  tools: string[];
  models: string[];
  tokens: number;
  count: number;
}

function hitPreviewOf(scope: AliasScope, identities: readonly IdentityUsage[]): AliasHitPreview {
  const matched = identities.filter((identity) => {
    if (scope.kind === "provider") {
      return identity.identity.observedProvider === scope.provider;
    }
    return (
      identity.identity.observedProvider === null &&
      identity.identity.sourceId === scope.sourceId &&
      identity.identity.modelId === scope.modelId
    );
  });
  return {
    tools: [...new Set(matched.map((identity) => identity.identity.sourceId))],
    models: [...new Set(matched.map((identity) => identity.identity.modelId))],
    tokens: matched.reduce(
      (sum, identity) => sum + identity.metrics.requestTotal + identity.buckets.output,
      0
    ),
    count: matched.length,
  };
}

export interface ProviderAliasDialogProps {
  open: boolean;
  onOpenChange(open: boolean): void;
  /** 点击行对应的候选身份（同名碰撞时包含全部候选，已由调用方按完整身份筛选） */
  identities: IdentityUsage[];
  /** 本行已显式绑定计费渠道：别名只影响其它未绑定记录 */
  explicitlyBound?: boolean;
  /** 当前生效的别名规则（用于预填与复用 id） */
  currentAliases: readonly ProviderAliasRule[];
  t: TFn;
}

export function ProviderAliasDialog({
  open,
  onOpenChange,
  identities,
  explicitlyBound = false,
  currentAliases,
  t,
}: ProviderAliasDialogProps) {
  const queryClient = useQueryClient();
  const lang = getLang();
  // 规则级命中预览与设置页 Manager 同源：近 30 天观测窗口、不叠加记录页筛选。
  // 恒挂载的宿主必须靠打开态门控查询，避免用户不打开也跑账本查询。
  const previewQuery = useUsageIdentitiesQuery({ preset: "last30d" }, {}, { enabled: open });
  const [selectedIndex, setSelectedIndex] = useState<number | null>(null);
  const [displayName, setDisplayName] = useState("");
  const [submitted, setSubmitted] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  // 打开时重置选择/草稿；候选只有一条时自动选中（多条必须显式选择，不静默选第一项）。
  // 依赖只留 open：打开期间 identities 引用随后台 refetch 变化不得清空已选候选与草稿。
  useEffect(() => {
    if (!open) return;
    setSelectedIndex(identities.length === 1 ? 0 : null);
    setDisplayName("");
    setSubmitted(false);
    setError(null);
    setBusy(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 意图=仅随 open 翻转重置
  }, [open]);

  const selected = selectedIndex === null ? null : (identities[selectedIndex] ?? null);
  const selectedKey = selected
    ? `${selected.identity.sourceId}|${selected.identity.observedProvider ?? ""}|${selected.identity.modelId}`
    : "";
  const existing = selected ? findAliasForIdentity(currentAliases, selected) : undefined;
  // 命中范围/Token 预览：规则键由所选身份决定（保存前即可见），
  // 数据来自与设置页 Manager 同源的「近 30 天观测」查询，按规则作用域过滤。
  const previewPool = previewQuery.data ?? [];
  const preview = selected ? hitPreviewOf(aliasScopeOfIdentity(selected), previewPool) : null;
  const previewRange = t("usage.alias.hitRangeValue", {
    range: t("usage.alias.usageWindow"),
  });

  // 选中身份变化时用已有别名预填；后台 refetch 换了 currentAliases 引用不得覆盖用户草稿。
  useEffect(() => {
    if (!open || !selected) return;
    setDisplayName(existing?.displayName ?? "");
    setSubmitted(false);
    setError(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 只在所选身份变化时预填
  }, [open, selectedKey]);

  const nameMissing = submitted && displayName.trim() === "";
  const canSave = selected !== null && !busy;

  const handleSave = async () => {
    setSubmitted(true);
    if (!selected || displayName.trim() === "") return;
    const scope = aliasScopeOfIdentity(selected);
    const rule: ProviderAliasRule = {
      id: existing?.id ?? crypto.randomUUID(),
      ...scope,
      displayName: displayName.trim(),
    };
    setBusy(true);
    setError(null);
    try {
      await usageProviderAliasUpsert(rule);
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
      onOpenChange(false);
    } catch (e) {
      // 失败保留草稿（含候选选择与显示名），由用户修正后重试
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] max-w-2xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t("usage.alias.title")}</DialogTitle>
          <DialogDescription>{t("usage.alias.desc")}</DialogDescription>
        </DialogHeader>

        <div className="space-y-4 text-sm">
          {identities.length === 0 ? (
            <p className="text-muted-foreground text-sm">{t("usage.empty")}</p>
          ) : null}

          {identities.length > 1 ? (
            <section className="space-y-1.5">
              <h3 className="text-muted-foreground text-xs font-medium">
                {t("usage.billing.candidates")}
              </h3>
              <div className="space-y-1">
                {identities.map((identity, i) => (
                  <button
                    key={`${identity.identity.sourceId}:${identity.identity.observedProvider ?? ""}:${identity.identity.modelId}:${i}`}
                    type="button"
                    data-testid={`alias-candidate-${i}`}
                    aria-pressed={selectedIndex === i}
                    className={`flex w-full items-center justify-between gap-2 rounded-md border px-2 py-1.5 text-left ${
                      selectedIndex === i ? "bg-accent text-accent-foreground" : ""
                    }`}
                    onClick={() => setSelectedIndex(i)}
                  >
                    <span className="min-w-0 truncate">{identityDisplayName(identity)}</span>
                    <span className="text-muted-foreground shrink-0 text-xs">
                      {usageAgentLabel(identity.identity.sourceId)} ·{" "}
                      {fmtTokens(identity.metrics.requestTotal + identity.buckets.output, lang)}
                    </span>
                  </button>
                ))}
              </div>
            </section>
          ) : null}

          {selected ? (
            <>
              <h3 className="text-muted-foreground text-xs font-medium">
                {t("usage.alias.readonlyIdentity")}
              </h3>
              <section
                data-testid="alias-readonly-identity"
                className="grid gap-1 rounded-md border p-2 text-xs sm:grid-cols-2"
              >
                <div>
                  <span className="text-muted-foreground">{t("usage.alias.sourceTool")}:</span>{" "}
                  {usageAgentLabel(selected.identity.sourceId)}
                </div>
                <div>
                  <span className="text-muted-foreground">{t("usage.alias.provider")}:</span>{" "}
                  {selected.identity.observedProvider ?? EM_DASH}
                </div>
                <div className="sm:col-span-2">
                  <span className="text-muted-foreground">{t("usage.alias.model")}:</span>{" "}
                  <span className="font-mono">{aliasModelLabel(selected.identity.modelId, t)}</span>
                </div>
                <div className="sm:col-span-2">
                  <span className="text-muted-foreground">{t("usage.alias.current")}:</span>{" "}
                  {existing?.displayName ?? EM_DASH}
                </div>
              </section>

              <p className="text-muted-foreground text-xs" data-testid="alias-scope-hint">
                {aliasScopeOfIdentity(selected).kind === "provider"
                  ? t("usage.alias.providerScopeHint")
                  : t("usage.alias.toolModelScopeHint")}
              </p>

              {explicitlyBound ? (
                <p
                  className="text-xs text-amber-600 dark:text-amber-400"
                  data-testid="alias-explicit-bound-hint"
                >
                  {t("usage.alias.explicitBoundHint")}
                </p>
              ) : null}

              {/* 保存前可见的命中预览（Spec §2.7）：只读、不落库；无命中出空态而不印 0。 */}
              <section
                data-testid="alias-hit-preview"
                className="grid gap-1 rounded-md border p-2 text-xs"
              >
                <h3 className="text-muted-foreground font-medium">{t("usage.alias.hitPreview")}</h3>
                {previewQuery.isPending ? (
                  <p className="text-muted-foreground" data-testid="alias-hit-loading">
                    {t("common.loading")}
                  </p>
                ) : preview && preview.count > 0 ? (
                  <>
                    <div>
                      <span className="text-muted-foreground">{t("usage.alias.hitTools")}:</span>{" "}
                      <span data-testid="alias-hit-tools">
                        {preview.tools.map((id) => usageAgentLabel(id)).join(", ")}
                      </span>
                    </div>
                    <div>
                      <span className="text-muted-foreground">{t("usage.alias.hitModels")}:</span>{" "}
                      <span data-testid="alias-hit-models">
                        {preview.models.map((modelId) => aliasModelLabel(modelId, t)).join(", ")}
                      </span>
                    </div>
                    <div>
                      <span className="text-muted-foreground">{t("usage.alias.hitTokens")}:</span>{" "}
                      <span data-testid="alias-hit-tokens" className="tabular-nums">
                        {fmtTokens(preview.tokens, lang)}
                      </span>
                    </div>
                  </>
                ) : (
                  <p className="text-muted-foreground" data-testid="alias-hit-empty">
                    {t("usage.alias.hitEmpty")}
                  </p>
                )}
                <div data-testid="alias-hit-range">
                  <span className="text-muted-foreground">{t("usage.alias.hitRange")}:</span>{" "}
                  {previewRange}
                </div>
              </section>

              <label className="flex items-center justify-between gap-2">
                <span>{t("usage.alias.displayName")}</span>
                <Input
                  data-testid="alias-display-name"
                  className="h-8 max-w-[18rem]"
                  value={displayName}
                  onChange={(e) => setDisplayName(e.currentTarget.value)}
                />
              </label>
              {nameMissing ? (
                <p className="text-destructive text-xs" data-testid="alias-name-required">
                  {t("usage.alias.nameRequired")}
                </p>
              ) : null}
            </>
          ) : identities.length > 1 ? (
            <p className="text-muted-foreground text-xs">{t("usage.billing.selectCandidate")}</p>
          ) : null}

          {error ? (
            <p data-testid="alias-error" className="text-destructive text-sm" role="alert">
              {usageErrMsg(error, t)}
            </p>
          ) : null}
        </div>

        <DialogFooter>
          <Button variant="outline" size="sm" onClick={() => onOpenChange(false)}>
            {t("common.cancel")}
          </Button>
          <Button
            size="sm"
            data-testid="alias-save-button"
            disabled={!canSave}
            onClick={() => void handleSave()}
          >
            {t("common.save")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

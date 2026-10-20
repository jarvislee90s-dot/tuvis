// 供应商别名的共享字段与「身份 → 规则键」工具（Task 7）。
//
// 本文件只做三件事：
//  1. 别名草稿的受控表单（类型 / 原始 provider / 工具 + 模型 / 显示名）——设置页管理器使用；
//  2. 把账本身份折叠成规则键（非空 provider → provider 规则；空 provider → toolModel 规则），
//     供记录页浮窗与行按钮判定复用；
//  3. 按规则键找已有别名（保存时复用 id，避免后端唯一索引拒绝）。
//
// 纪律：别名只影响展示/分组；本文件不读写账本、不发 IPC、不新增 i18n 键。
import { Input } from "@/components/ui/input";
import { FILTER_TOOL_IDS } from "@/lib/usage/useRecordsFilters";
import { usageAgentLabel } from "@/lib/usage/format";
import type { TFn } from "@/lib/usage/range";
import type { UsageSourceId } from "@/types/usage";
import type { IdentityUsage, ProviderAliasKind, ProviderAliasRule } from "@/types/usageV2";

/** 一条别名的「目标键」部分（不含 id / 显示名） */
export type AliasScope = Pick<ProviderAliasRule, "kind" | "provider" | "sourceId" | "modelId">;

/** 表单草稿：与规则的目标键同形，但输入侧用空串表达「未填」，提交时再折叠成 null */
export interface AliasDraft {
  kind: ProviderAliasKind;
  provider: string;
  sourceId: UsageSourceId;
  modelId: string;
  displayName: string;
}

/** 新建草稿的初始值：provider 规则，工具默认七源第一项（toolModel 分支必须有合法 sourceId） */
export const EMPTY_ALIAS_DRAFT: AliasDraft = {
  kind: "provider",
  provider: "",
  sourceId: "claude",
  modelId: "",
  displayName: "",
};

/**
 * 规则键的稳定字符串表示（只用于前端去重 / 找已有规则，不落库）：
 *  * `provider|<原始串>`
 *  * `toolModel|<sourceId>|<modelId>`（`modelId=""` 是合法的“未知模型”）
 */
export function aliasKeyOf(scope: AliasScope): string {
  return scope.kind === "provider"
    ? `provider|${scope.provider ?? ""}`
    : `toolModel|${scope.sourceId ?? ""}|${scope.modelId ?? ""}`;
}

/**
 * 账本身份 → 别名规则键（与后端 `ProviderAliasResolver` 口径一致）：
 *  * 非空 provider：作用于该原始 provider 的所有工具/模型；
 *  * 空 provider：只作用于该 `sourceId + modelId`（`modelId` 可为空串）。
 */
export function aliasScopeOfIdentity(identity: IdentityUsage): AliasScope {
  const provider = identity.identity.observedProvider;
  if (provider !== null && provider !== "") {
    return { kind: "provider", provider, sourceId: null, modelId: null };
  }
  return {
    kind: "toolModel",
    provider: null,
    sourceId: identity.identity.sourceId,
    modelId: identity.identity.modelId,
  };
}

/** 按身份找已有别名规则（同键唯一；找不到返回 undefined）。 */
export function findAliasForIdentity(
  aliases: readonly ProviderAliasRule[],
  identity: IdentityUsage
): ProviderAliasRule | undefined {
  const key = aliasKeyOf(aliasScopeOfIdentity(identity));
  return aliases.find((rule) => aliasKeyOf(rule) === key);
}

/**
 * 记录页行显示名：供应商别名优先，模型名仍用原始值。
 *
 *  * 非空 provider：`别名 / 模型`，无别名时 `原始 provider / 模型`；
 *  * 空 provider：有 toolModel 别名时 `别名 / 模型`，否则退化为裸模型名（与后端
 *    `route_label` 的原始口径一致）。
 *
 * 这里只做展示；别名不写账本，行数据里的原始身份也不变。
 */
export function displayIdentityLabel(
  aliases: readonly ProviderAliasRule[],
  identity: IdentityUsage
): string {
  const provider = identity.identity.observedProvider;
  const displayProvider = findAliasForIdentity(aliases, identity)?.displayName ?? provider;
  const model = identity.identity.modelId;
  return displayProvider ? `${displayProvider} / ${model}` : model;
}

/** 按目标键找已有别名规则（设置页新增时避免撞唯一索引；同键视为覆盖更新）。 */
export function findAliasForScope(
  aliases: readonly ProviderAliasRule[],
  scope: AliasScope
): ProviderAliasRule | undefined {
  const key = aliasKeyOf(scope);
  return aliases.find((rule) => aliasKeyOf(rule) === key);
}

/** 模型为空串 = 未知模型；展示层唯一口径。 */
export function aliasModelLabel(modelId: string | null, t: TFn): string {
  return modelId === null || modelId === "" ? t("usage.label.unknownModel") : modelId;
}

export interface ProviderAliasFieldsProps {
  draft: AliasDraft;
  onChange(patch: Partial<AliasDraft>): void;
  t: TFn;
  /** 提交过一次后才显示必填原因（避免一打开就满屏红字） */
  submitted: boolean;
  disabled?: boolean;
}

/** 别名目标 + 显示名的受控字段组；设置页新增/编辑共用。 */
export function ProviderAliasFields({
  draft,
  onChange,
  t,
  submitted,
  disabled = false,
}: ProviderAliasFieldsProps) {
  const missingName = submitted && draft.displayName.trim() === "";
  const missingProvider = submitted && draft.kind === "provider" && draft.provider.trim() === "";

  return (
    <div className="space-y-2" data-testid="provider-alias-fields">
      <label className="flex items-center justify-between gap-2">
        <span>{t("usage.alias.kind")}</span>
        <select
          data-testid="provider-alias-kind"
          className="bg-background h-8 rounded-md border px-2 text-sm"
          value={draft.kind}
          disabled={disabled}
          onChange={(e) => onChange({ kind: e.currentTarget.value as ProviderAliasKind })}
        >
          <option value="provider">{t("usage.alias.kindProvider")}</option>
          <option value="toolModel">{t("usage.alias.kindToolModel")}</option>
        </select>
      </label>

      {draft.kind === "provider" ? (
        <>
          <label className="flex items-center justify-between gap-2">
            <span>{t("usage.alias.provider")}</span>
            <Input
              data-testid="provider-alias-provider"
              className="h-8 max-w-[18rem]"
              value={draft.provider}
              disabled={disabled}
              onChange={(e) => onChange({ provider: e.currentTarget.value })}
            />
          </label>
          {missingProvider ? (
            <p className="text-destructive text-xs" data-testid="provider-alias-provider-required">
              {t("usage.alias.providerRequired")}
            </p>
          ) : null}
          <p className="text-muted-foreground text-xs">{t("usage.alias.providerScopeHint")}</p>
        </>
      ) : (
        <>
          <label className="flex items-center justify-between gap-2">
            <span>{t("usage.alias.sourceTool")}</span>
            <select
              data-testid="provider-alias-source"
              className="bg-background h-8 rounded-md border px-2 text-sm"
              value={draft.sourceId ?? "claude"}
              disabled={disabled}
              onChange={(e) => onChange({ sourceId: e.currentTarget.value as UsageSourceId })}
            >
              {FILTER_TOOL_IDS.map((id) => (
                <option key={id} value={id}>
                  {usageAgentLabel(id)}
                </option>
              ))}
            </select>
          </label>
          <label className="flex items-center justify-between gap-2">
            <span>{t("usage.alias.model")}</span>
            <Input
              data-testid="provider-alias-model"
              className="h-8 max-w-[18rem]"
              value={draft.modelId ?? ""}
              disabled={disabled}
              placeholder={t("usage.label.unknownModel")}
              onChange={(e) => onChange({ modelId: e.currentTarget.value })}
            />
          </label>
          <p className="text-muted-foreground text-xs">{t("usage.alias.toolModelScopeHint")}</p>
        </>
      )}

      <label className="flex items-center justify-between gap-2">
        <span>{t("usage.alias.displayName")}</span>
        <Input
          data-testid="provider-alias-display-name"
          className="h-8 max-w-[18rem]"
          value={draft.displayName}
          disabled={disabled}
          onChange={(e) => onChange({ displayName: e.currentTarget.value })}
        />
      </label>
      {missingName ? (
        <p className="text-destructive text-xs" data-testid="provider-alias-name-required">
          {t("usage.alias.nameRequired")}
        </p>
      ) : null}
    </div>
  );
}

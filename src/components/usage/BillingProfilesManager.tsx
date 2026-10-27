// 计费渠道与规则管理（Task 13）。Props 只有 `{t}`；内部读取 billing 配置并直接提交候选。
//
// 支持：新建/改名/归档渠道；编辑规则目标/日期/前缀；停用规则（移出当前快照）；
// 显示冲突/含糊；按目标 revision 撤销（0 = 空配置）。渠道不物理删除。
// 参考 API 模型编辑留到 Task 17（这里只放占位说明，不调用未定义接口）。
import { useEffect, useState, type KeyboardEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { usageBillingCommit, usageBillingUndo } from "@/lib/api/usageV2";
import {
  USAGE_KEY,
  useUsageBillingQuery,
  useUsageIdentitiesQuery,
  useUsagePricingCatalogQuery,
  useUsagePricingEvents,
  useUsagePricingQuery,
} from "@/lib/query/queries/usageV2";
import { EM_DASH, fmtDateTime, usageAgentLabel } from "@/lib/usage/format";
import type { TFn } from "@/lib/usage/range";
import { isUsageRpcError, usageErrMsg } from "@/components/usage/usageErrors";
import type { BillingConfig, BillingRule, PricingCatalogEntry, PricingRule } from "@/types/usageV2";

export interface BillingProfilesManagerProps {
  t: TFn;
  /** 受控打开态（记录页页脚「高级：计费归属与参考价规则」用）；不传则组件自己持有 */
  open?: boolean;
  onOpenChange?(open: boolean): void;
  /** 受控模式下可隐藏自带的触发按钮 */
  hideTrigger?: boolean;
  /**
   * active 门控（评审修复轮 2）：组件随记录面板在四页宿主里常驻挂载，页面隐藏期间传
   * false——billing 读查询停发（含 focus refetch）。保持「弹窗关着也常驻查」的既有语义，
   * 只叠加页签级门控；默认 true，旧调用者行为不变。第三/四轮评审 follow-up：弹窗内的
   * identities / pricing / catalog 查询同样叠上 active 门控。
   */
  active?: boolean;
}

function identityKeyOf(rule: { identity: BillingRule["identity"] }): string {
  const { sourceId, observedProvider, modelId } = rule.identity;
  return `${sourceId}|${observedProvider ?? ""}|${modelId}`;
}

function identityLabelOf(rule: { identity: BillingRule["identity"] }): string {
  const { sourceId, observedProvider, modelId } = rule.identity;
  // 明确展示 source / provider / model 三段（provider 缺失也显示占位）
  return `${usageAgentLabel(sourceId)} · ${observedProvider ?? EM_DASH} / ${modelId}`;
}

function dateToMs(date: string): number | null {
  if (!date) return null;
  const [y, m, d] = date.split("-").map(Number);
  if (!y || !m || !d) return null;
  return new Date(y, m - 1, d).getTime();
}

function msToDate(ms: number | null): string {
  if (ms === null) return "";
  const d = new Date(ms);
  const p = (v: number) => String(v).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/**
 * 按后端 detail 区分「无有效快照/目录不可用」与「参考键不存在」。
 *
 * 依赖的后端 detail 字符串清单（后端改文案必须同步本函数；匹配用**前缀逐字**对齐，
 * 不再用宽泛正则）。全部经 `usage_billing_commit` 在 pricingRules 变更时返回，
 * 来源 = `src-tauri/src/services/usage/v2/billing.rs` `validate_pricing_references`
 * 及其调用的 `pricing::load_active_catalog_with_conn`：
 *
 *  1. `pricing catalog unavailable`
 *     —— 存在新增/变更参考规则但无 active 目录（billing.rs，码 usage-settings-invalid）
 *     → pricingNeedSnapshot；
 *  2. `pricing reference not found: <providerId>/<modelId>`
 *     —— 新增/变更引用键不在目录精确键中（billing.rs，码 usage-settings-invalid）
 *     → pricingKeyMissing；
 *  3. `pricing catalog: …`（parse_catalog 失败：invalid JSON / top-level 非空对象 /
 *     cost 非数字 / payload 非 UTF-8 等，经 load_active_catalog_with_conn `?` 传播）
 *     —— 同属「目录不可用」面 → pricingNeedSnapshot；
 *  4. 其余（revision 冲突、配置校验等与定价目录无关的失败）→ 兜底 pricingNeedEnable。
 */
function pricingHintText(error: unknown, t: TFn): string {
  const detail = isUsageRpcError(error) ? (error.detail ?? "") : "";
  if (detail.startsWith("pricing catalog")) {
    return t("usage.billing.pricingNeedSnapshot");
  }
  if (detail.startsWith("pricing reference not found")) {
    return t("usage.billing.pricingKeyMissing");
  }
  return t("usage.billing.pricingNeedEnable");
}

function ruleEnd(rule: BillingRule): number {
  return rule.effectiveTo ?? Number.POSITIVE_INFINITY;
}

function intervalsOverlap(a: BillingRule, b: BillingRule): boolean {
  return a.effectiveFrom < ruleEnd(b) && b.effectiveFrom < ruleEnd(a);
}

/** 严格更窄：a 完整包含 b 且不等于 b（更窄区间支配更宽区间）。 */
function strictlyContains(a: BillingRule, b: BillingRule): boolean {
  return (
    a.effectiveFrom <= b.effectiveFrom &&
    ruleEnd(a) >= ruleEnd(b) &&
    (a.effectiveFrom < b.effectiveFrom || ruleEnd(a) > ruleEnd(b))
  );
}

/**
 * 与后端归因语义对齐的**近似**冲突检测：
 *  * 不同 rank（exact vs prefix）不冲突；
 *  * 生效区间不重叠不冲突；
 *  * 同 rank、重叠、目标不同：若一方严格包含另一方，更窄区间支配，不冲突；
 *    交叉或完全相同 ⇒ 冲突。
 *  * `mixed` 单独标记含糊。
 *
 * 后端还会处理前缀长度与时间边界的组合优先级；这里无法逐桶镜像，界面提示以「可能存在冲突，
 * 以预览为准」呈现（见 i18n `usage.billing.conflict`）。
 */
export function ruleIssues(rules: readonly BillingRule[]): {
  conflicts: string[];
  ambiguous: string[];
} {
  const byIdentity = new Map<string, BillingRule[]>();
  for (const rule of rules) {
    const key = identityKeyOf(rule);
    byIdentity.set(key, [...(byIdentity.get(key) ?? []), rule]);
  }
  const conflicts = new Set<string>();
  const ambiguous = new Set<string>();
  for (const list of byIdentity.values()) {
    if (list.some((rule) => rule.mixed)) ambiguous.add(identityLabelOf(list[0]));
    for (let i = 0; i < list.length; i += 1) {
      for (let j = i + 1; j < list.length; j += 1) {
        const a = list[i];
        const b = list[j];
        if (a.match !== b.match) continue; // 不同 rank 不冲突
        if (a.billingProfileId === b.billingProfileId) continue;
        if (!intervalsOverlap(a, b)) continue;
        if (strictlyContains(a, b) || strictlyContains(b, a)) continue; // 更窄区间支配
        conflicts.add(identityLabelOf(a));
      }
    }
  }
  return { conflicts: [...conflicts], ambiguous: [...ambiguous] };
}

export function BillingProfilesManager({
  t,
  open,
  onOpenChange,
  hideTrigger = false,
  active = true,
}: BillingProfilesManagerProps) {
  const queryClient = useQueryClient();
  const billing = useUsageBillingQuery({ enabled: active });
  const [internalOpen, setInternalOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [undoTarget, setUndoTarget] = useState("0");
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);
  /** 渠道名受控草稿：仅随 billing revision 变化整体重置（见下方 effect），提交失败时 service 值会盖回旧值 */
  const [nameDrafts, setNameDrafts] = useState<Record<string, string>>({});
  /** 已据此重建过草稿的 billing revision；后台 refetch 未换版本不得清空输入中的草稿 */
  const [nameDraftsRevision, setNameDraftsRevision] = useState<number | null>(null);
  /** 参考 API 模型草稿（失败保留，成功才清空） */
  const [pricingIdentity, setPricingIdentity] = useState("");
  const [pricingProviderId, setPricingProviderId] = useState("");
  const [pricingModelId, setPricingModelId] = useState("");
  const [pricingAuthorized, setPricingAuthorized] = useState(false);
  const [pricingError, setPricingError] = useState<unknown>(null);
  /** 目录搜索：查询串、结果展开态与键盘高亮下标 */
  const [catalogQuery, setCatalogQuery] = useState("");
  const [catalogOpen, setCatalogOpen] = useState(false);
  const [catalogActive, setCatalogActive] = useState(-1);

  const controlled = open !== undefined;
  const dialogOpen = controlled ? open : internalOpen;
  const setDialogOpen = (next: boolean) => {
    if (controlled) onOpenChange?.(next);
    else setInternalOpen(next);
  };

  /** 参考模型可选身份 = 账本观测身份（7 天窗口）+ 现有规则身份。
   *  本组件被费用卡 / 记录页**恒挂载**，查询以「对话框打开且页面 active」为门控，
   *  否则用户从不打开管理器也会常驻跑 7 天账本查询。 */
  const observed = useUsageIdentitiesQuery(
    { preset: "last7d" },
    {},
    { enabled: dialogOpen && active }
  );
  // 目录搜索只在对话框打开、页面 active 且存在 active 快照时启用（无快照时后端必然返回空）。
  const pricingStatus = useUsagePricingQuery({ enabled: dialogOpen && active });
  // 刷新成功/失败后失效 pricing：缓存（尤其是「无快照」）不再长期挡住目录搜索。
  useUsagePricingEvents();
  const refetchPricing = pricingStatus.refetch;
  // 打开对话框时强制重取 pricing：真实库可能已有快照，而前端缓存仍是 30s 内（或禁用期）的旧空状态。
  useEffect(() => {
    if (dialogOpen) void refetchPricing();
  }, [dialogOpen, refetchPricing]);
  const hasPricingSnapshot = (pricingStatus.data?.snapshotId ?? null) !== null;
  const catalog = useUsagePricingCatalogQuery(catalogQuery, {
    enabled: dialogOpen && active && hasPricingSnapshot,
  });
  const catalogResults = (catalog.data ?? []).slice(0, 20);

  useEffect(() => {
    const data = billing.data;
    // 仅随 revision 变化重建草稿：采集 ticker 的 invalidateQueries 会改变 data 引用，
    // 若按引用重建会把用户输入中的改名草稿清空（与 3b5c1cc3「选中保持」同类问题）。
    if (!data || data.revision === nameDraftsRevision) return;
    setNameDraftsRevision(data.revision);
    setNameDrafts(Object.fromEntries(data.profiles.map((p) => [p.id, p.name])));
  }, [billing.data, nameDraftsRevision]);

  // 身份 / 参考键任一项变化：授权失效、旧错误清空、保存再次禁用
  useEffect(() => {
    setPricingAuthorized(false);
    setPricingError(null);
  }, [pricingIdentity, pricingProviderId, pricingModelId]);

  // 结果集变化后重置键盘高亮（避免指向已不存在的行）
  useEffect(() => {
    setCatalogActive(-1);
  }, [catalogQuery, catalog.data]);

  const selectCatalogEntry = (entry: PricingCatalogEntry) => {
    // 只回填两个精确键输入框；授权仍由用户显式勾选（上方 effect 会清掉旧的授权）。
    setPricingProviderId(entry.providerId);
    setPricingModelId(entry.modelId);
    setCatalogOpen(false);
    setCatalogActive(-1);
  };

  const handleCatalogKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      if (catalogResults.length === 0) return;
      event.preventDefault();
      event.stopPropagation();
      setCatalogOpen(true);
      setCatalogActive((prev) => (prev < 0 ? 0 : Math.min(prev + 1, catalogResults.length - 1)));
    } else if (event.key === "ArrowUp") {
      if (catalogResults.length === 0) return;
      event.preventDefault();
      event.stopPropagation();
      setCatalogActive((prev) => (prev <= 0 ? 0 : prev - 1));
    } else if (event.key === "Enter") {
      const entry = catalogOpen ? catalogResults[catalogActive] : undefined;
      if (entry) {
        event.preventDefault();
        event.stopPropagation();
        selectCatalogEntry(entry);
      }
    } else if (event.key === "Escape") {
      // 只收起下拉，不让 Escape 冒泡到 Dialog 把整个管理器关掉。
      event.preventDefault();
      event.stopPropagation();
      setCatalogOpen(false);
      setCatalogActive(-1);
    }
  };

  const resetNameDrafts = () => {
    if (!billing.data) return;
    setNameDrafts(Object.fromEntries(billing.data.profiles.map((p) => [p.id, p.name])));
  };

  const commit = async (next: BillingConfig): Promise<{ ok: boolean; error?: unknown }> => {
    setBusy(true);
    setError(null);
    try {
      await usageBillingCommit({
        expectedRevision: next.revision,
        candidate: { ...next, revision: next.revision },
      });
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
      return { ok: true };
    } catch (e) {
      setError(e);
      // 提交失败：立即把受控草稿回滚到服务端仍生效的旧值（不等待重拉）
      resetNameDrafts();
      void billing.refetch();
      return { ok: false, error: e };
    } finally {
      setBusy(false);
    }
  };

  const mutate = (fn: (config: BillingConfig) => BillingConfig) => {
    if (!billing.data) return;
    void commit(fn(billing.data));
  };

  const handleUndo = async () => {
    if (!billing.data) return;
    const target = Number.parseInt(undoTarget, 10);
    if (!Number.isFinite(target) || target < 0) return;
    setBusy(true);
    setError(null);
    try {
      await usageBillingUndo(billing.data.revision, target);
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const data = billing.data;
  const issues = data ? ruleIssues(data.rules) : { conflicts: [], ambiguous: [] };

  /** 可绑定的原日志身份：账本观测身份 + 现有渠道/参考规则身份，按完整 IdentityKey 去重 */
  const identityOptions: { key: string; identity: BillingRule["identity"] }[] = [];
  const addIdentity = (identity: BillingRule["identity"]) => {
    const key = identityKeyOf({ identity });
    if (!identityOptions.some((option) => option.key === key)) {
      identityOptions.push({ key, identity });
    }
  };
  for (const identity of observed.data ?? []) addIdentity(identity.identity);
  if (data) {
    for (const item of [...data.rules, ...data.pricingRules]) addIdentity(item.identity);
  }
  const selectedIdentity =
    pricingIdentity === ""
      ? null
      : (identityOptions.find((option) => option.key === pricingIdentity) ?? null);
  const pricingReady =
    selectedIdentity !== null &&
    pricingProviderId.trim() !== "" &&
    pricingModelId.trim() !== "" &&
    pricingAuthorized;

  const savePricingRule = async () => {
    if (!data || !selectedIdentity || !pricingReady) return;
    const pricingRule: PricingRule = {
      id: crypto.randomUUID(),
      // 精确键；复制完整身份，不读 UI 文本
      identity: { ...selectedIdentity.identity },
      match: "exact",
      reference: {
        providerId: pricingProviderId.trim(),
        modelId: pricingModelId.trim(),
      },
    };
    setPricingError(null);
    const result = await commit({
      ...data,
      pricingRules: [...data.pricingRules, pricingRule],
    });
    if (result.ok) {
      setPricingIdentity("");
      setPricingProviderId("");
      setPricingModelId("");
      setPricingAuthorized(false);
    } else {
      // 后端在 pricingRules 变更时校验 active catalog：保留草稿供用户改键后重试
      setPricingError(result.error ?? { code: "usage-settings-invalid", detail: "" });
    }
  };

  return (
    <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
      {hideTrigger ? null : (
        <DialogTrigger asChild>
          <Button variant="outline" size="sm" data-testid="billing-manager-open">
            {t("usage.billing.manage")}
          </Button>
        </DialogTrigger>
      )}
      <DialogContent
        className="max-h-[85vh] max-w-3xl overflow-y-auto"
        // 下拉展开时 Escape 只收下拉（Radix 的 document 监听先于输入框处理器）；
        // 下拉收起后 Escape 仍可关闭整个管理器。
        onEscapeKeyDown={(event) => {
          if (catalogOpen) {
            event.preventDefault();
            setCatalogOpen(false);
            setCatalogActive(-1);
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>{t("usage.billing.manageTitle")}</DialogTitle>
          <DialogDescription>{t("usage.billing.manageDesc")}</DialogDescription>
        </DialogHeader>

        {billing.isPending ? (
          <p className="text-muted-foreground text-sm">{t("common.loading")}</p>
        ) : billing.isError ? (
          <div className="space-y-2" role="alert">
            <p className="text-destructive text-sm">{usageErrMsg(billing.error, t)}</p>
            <Button size="sm" variant="outline" onClick={() => void billing.refetch()}>
              {t("common.retry")}
            </Button>
          </div>
        ) : data ? (
          <div className="space-y-5 text-sm">
            <p className="text-muted-foreground text-xs" data-testid="billing-revision">
              {t("usage.billing.revision", { n: data.revision })}
            </p>

            {error ? (
              <p
                data-testid="billing-manager-error"
                className="text-destructive text-sm"
                role="alert"
              >
                {usageErrMsg(error, t)}
              </p>
            ) : null}

            {/* 渠道 */}
            <section className="space-y-2">
              <h3 className="text-muted-foreground text-xs font-medium">
                {t("usage.billing.profiles")}
              </h3>
              {data.profiles.length === 0 ? (
                <p className="text-muted-foreground text-xs">{t("usage.billing.emptyProfiles")}</p>
              ) : (
                <ul className="space-y-1.5">
                  {data.profiles.map((profile) => (
                    <li
                      key={profile.id}
                      data-testid={`billing-profile-${profile.id}`}
                      className="flex items-center gap-2"
                    >
                      <Input
                        aria-label={t("usage.billing.name")}
                        data-testid={`billing-profile-name-${profile.id}`}
                        className="h-8"
                        value={nameDrafts[profile.id] ?? profile.name}
                        disabled={profile.archived || busy}
                        onChange={(e) =>
                          setNameDrafts((drafts) => ({
                            ...drafts,
                            [profile.id]: e.target.value,
                          }))
                        }
                        onBlur={(e) => {
                          const name = e.target.value.trim();
                          if (name === "" || name === profile.name) {
                            setNameDrafts((drafts) => ({
                              ...drafts,
                              [profile.id]: profile.name,
                            }));
                            return;
                          }
                          mutate((cfg) => ({
                            ...cfg,
                            profiles: cfg.profiles.map((p) =>
                              p.id === profile.id ? { ...p, name } : p
                            ),
                          }));
                        }}
                      />
                      {profile.archived ? (
                        <span className="text-muted-foreground shrink-0 text-xs">
                          {t("usage.billing.archived")}
                        </span>
                      ) : null}
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={busy}
                        data-testid={`billing-profile-archive-${profile.id}`}
                        onClick={() =>
                          mutate((cfg) => ({
                            ...cfg,
                            profiles: cfg.profiles.map((p) =>
                              p.id === profile.id ? { ...p, archived: !p.archived } : p
                            ),
                          }))
                        }
                      >
                        {profile.archived
                          ? t("usage.billing.unarchive")
                          : t("usage.billing.archive")}
                      </Button>
                    </li>
                  ))}
                </ul>
              )}

              <div className="flex items-center gap-2">
                <Input
                  data-testid="billing-new-profile"
                  className="h-8"
                  placeholder={t("usage.billing.newProfilePlaceholder")}
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                />
                <Button
                  size="sm"
                  data-testid="billing-create-profile"
                  disabled={busy || newName.trim() === ""}
                  onClick={() => {
                    const name = newName.trim();
                    if (!name) return;
                    mutate((cfg) => ({
                      ...cfg,
                      profiles: [
                        ...cfg.profiles,
                        {
                          id: crypto.randomUUID(),
                          name,
                          kind: "unknown",
                          note: "",
                          archived: false,
                        },
                      ],
                    }));
                    setNewName("");
                  }}
                >
                  {t("usage.billing.create")}
                </Button>
              </div>
            </section>

            {/* 规则 */}
            <section className="space-y-2">
              <h3 className="text-muted-foreground text-xs font-medium">
                {t("usage.billing.rules")}
              </h3>
              {data.rules.length === 0 ? (
                <p className="text-muted-foreground text-xs">{t("usage.billing.emptyRules")}</p>
              ) : (
                <ul className="space-y-2">
                  {data.rules.map((rule) => (
                    <li
                      key={rule.id}
                      data-testid={`billing-rule-${rule.id}`}
                      className="space-y-1.5 rounded-md border p-2"
                    >
                      <div className="text-xs">
                        <span className="text-muted-foreground">
                          {t("usage.billing.identity")}:
                        </span>{" "}
                        {identityLabelOf(rule)}
                      </div>
                      <div className="flex flex-wrap items-center gap-2">
                        <select
                          aria-label={t("usage.billing.target")}
                          data-testid={`billing-rule-target-${rule.id}`}
                          className="bg-background h-8 rounded-md border px-2 text-sm"
                          value={rule.billingProfileId}
                          disabled={busy}
                          onChange={(e) =>
                            mutate((cfg) => ({
                              ...cfg,
                              rules: cfg.rules.map((r) =>
                                r.id === rule.id ? { ...r, billingProfileId: e.target.value } : r
                              ),
                            }))
                          }
                        >
                          {data.profiles.map((p) => (
                            <option key={p.id} value={p.id} disabled={p.archived}>
                              {p.name}
                              {p.archived ? ` (${t("usage.billing.archived")})` : ""}
                            </option>
                          ))}
                        </select>
                        <select
                          aria-label={t("usage.billing.match")}
                          data-testid={`billing-rule-match-${rule.id}`}
                          className="bg-background h-8 rounded-md border px-2 text-sm"
                          value={rule.match}
                          disabled={busy}
                          onChange={(e) =>
                            mutate((cfg) => ({
                              ...cfg,
                              rules: cfg.rules.map((r) =>
                                r.id === rule.id
                                  ? { ...r, match: e.target.value as "exact" | "prefix" }
                                  : r
                              ),
                            }))
                          }
                        >
                          <option value="exact">{t("usage.billing.exact")}</option>
                          <option value="prefix">{t("usage.billing.prefix")}</option>
                        </select>
                        <label className="text-muted-foreground flex items-center gap-1 text-xs">
                          {t("usage.billing.effectiveFromShort")}
                          <input
                            type="date"
                            data-testid={`billing-rule-from-${rule.id}`}
                            className="bg-background h-8 rounded-md border px-2 text-xs"
                            value={msToDate(rule.effectiveFrom)}
                            disabled={busy}
                            onChange={(e) => {
                              const ms = dateToMs(e.target.value);
                              if (ms === null) return;
                              mutate((cfg) => ({
                                ...cfg,
                                rules: cfg.rules.map((r) =>
                                  r.id === rule.id ? { ...r, effectiveFrom: ms } : r
                                ),
                              }));
                            }}
                          />
                        </label>
                        <label className="text-muted-foreground flex items-center gap-1 text-xs">
                          {t("usage.billing.effectiveToShort")}
                          <input
                            type="date"
                            data-testid={`billing-rule-to-${rule.id}`}
                            className="bg-background h-8 rounded-md border px-2 text-xs"
                            value={msToDate(rule.effectiveTo)}
                            disabled={busy}
                            onChange={(e) => {
                              const ms = dateToMs(e.target.value);
                              mutate((cfg) => ({
                                ...cfg,
                                rules: cfg.rules.map((r) =>
                                  r.id === rule.id ? { ...r, effectiveTo: ms } : r
                                ),
                              }));
                            }}
                          />
                        </label>
                        <Button
                          size="sm"
                          variant="outline"
                          disabled={busy}
                          data-testid={`billing-rule-remove-${rule.id}`}
                          onClick={() =>
                            mutate((cfg) => ({
                              ...cfg,
                              rules: cfg.rules.filter((r) => r.id !== rule.id),
                            }))
                          }
                        >
                          {t("usage.billing.disableRule")}
                        </Button>
                      </div>
                      <p className="text-muted-foreground text-[11px]">
                        {t("usage.billing.effectiveRange", {
                          from: fmtDateTime(rule.effectiveFrom),
                          to: rule.effectiveTo === null ? EM_DASH : fmtDateTime(rule.effectiveTo),
                        })}
                      </p>
                    </li>
                  ))}
                </ul>
              )}
            </section>

            {/* 参考 API 模型（Task 17）：只接受明确 providerId + modelId 精确键；
                需用户显式授权，不按相似名自动匹配，也不接受月费换算。 */}
            <section className="space-y-2">
              <h3 className="text-muted-foreground text-xs font-medium">
                {t("usage.billing.pricingRules")}
              </h3>
              <p className="text-muted-foreground text-xs">{t("usage.billing.pricingDesc")}</p>

              {data.pricingRules.length > 0 ? (
                <ul className="space-y-1 text-xs">
                  {data.pricingRules.map((pricingRule) => (
                    <li
                      key={pricingRule.id}
                      data-testid={`pricing-rule-${pricingRule.id}`}
                      className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-2"
                    >
                      <span>
                        {identityLabelOf(pricingRule)} →{" "}
                        <span className="font-mono">
                          {pricingRule.reference.providerId} / {pricingRule.reference.modelId}
                        </span>
                      </span>
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={busy}
                        data-testid={`pricing-rule-remove-${pricingRule.id}`}
                        onClick={() =>
                          mutate((cfg) => ({
                            ...cfg,
                            pricingRules: cfg.pricingRules.filter((r) => r.id !== pricingRule.id),
                          }))
                        }
                      >
                        {t("usage.billing.pricingRemove")}
                      </Button>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-muted-foreground text-xs">{t("usage.billing.pricingEmpty")}</p>
              )}

              <div className="space-y-1.5 rounded-md border p-2">
                <label className="flex flex-wrap items-center gap-2 text-xs">
                  <span className="text-muted-foreground">
                    {t("usage.billing.pricingIdentity")}
                  </span>
                  <select
                    data-testid="pricing-identity-select"
                    className="bg-background h-8 min-w-[14rem] rounded-md border px-2 text-sm"
                    value={pricingIdentity}
                    onChange={(e) => setPricingIdentity(e.target.value)}
                  >
                    <option value="">{t("usage.billing.pricingSelectIdentity")}</option>
                    {identityOptions.map((option) => (
                      <option key={option.key} value={option.key}>
                        {identityLabelOf({ identity: option.identity })}
                      </option>
                    ))}
                  </select>
                </label>

                {/* 目录搜索：输入 ≥2 字符后展示结果，选中自动回填 provider/model。 */}
                <div
                  data-testid="pricing-catalog"
                  className="relative space-y-1"
                  onBlur={(event) => {
                    if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
                      setCatalogOpen(false);
                    }
                  }}
                >
                  <Input
                    data-testid="pricing-catalog-search"
                    className="h-8"
                    role="combobox"
                    aria-expanded={catalogOpen && catalogResults.length > 0}
                    aria-controls="pricing-catalog-listbox"
                    aria-autocomplete="list"
                    aria-label={t("usage.billing.pricingCatalogSearch")}
                    placeholder={t("usage.billing.pricingCatalogSearch")}
                    value={catalogQuery}
                    onChange={(event) => {
                      const value = event.target.value;
                      setCatalogQuery(value);
                      // 展开态只取决于输入长度；无快照由独立提示分支处理。
                      setCatalogOpen(value.trim().length >= 2);
                      setCatalogActive(-1);
                    }}
                    onFocus={() => {
                      if (catalogQuery.trim().length >= 2) {
                        setCatalogOpen(true);
                      }
                    }}
                    onKeyDown={handleCatalogKeyDown}
                  />

                  {!hasPricingSnapshot ? (
                    <p
                      data-testid="pricing-catalog-no-snapshot"
                      className="text-muted-foreground text-[11px]"
                    >
                      {t("usage.billing.pricingCatalogNoSnapshot")}
                    </p>
                  ) : catalogQuery.trim().length < 2 ? (
                    <p className="text-muted-foreground text-[11px]">
                      {t("usage.billing.pricingCatalogHint")}
                    </p>
                  ) : catalog.isError ? (
                    <div
                      data-testid="pricing-catalog-error"
                      role="alert"
                      className="flex flex-wrap items-center gap-2"
                    >
                      <p className="text-destructive text-[11px]">
                        {usageErrMsg(catalog.error, t)}
                      </p>
                      <Button
                        size="sm"
                        variant="outline"
                        data-testid="pricing-catalog-retry"
                        onClick={() => void catalog.refetch()}
                      >
                        {t("common.retry")}
                      </Button>
                    </div>
                  ) : catalog.isPending ? (
                    <p className="text-muted-foreground text-[11px]">{t("common.loading")}</p>
                  ) : catalogOpen && catalogResults.length > 0 ? (
                    <ul
                      id="pricing-catalog-listbox"
                      data-testid="pricing-catalog-results"
                      role="listbox"
                      className="bg-background absolute z-50 mt-1 max-h-64 w-full overflow-y-auto rounded-md border shadow-md"
                    >
                      {catalogResults.map((entry, index) => (
                        <li
                          key={`${entry.providerId}/${entry.modelId}`}
                          role="option"
                          aria-selected={index === catalogActive}
                          data-testid={`pricing-catalog-option-${entry.providerId}/${entry.modelId}`}
                          className={`cursor-pointer px-2 py-1.5 text-xs ${
                            index === catalogActive ? "bg-accent text-accent-foreground" : ""
                          }`}
                          onMouseDown={(event) => event.preventDefault()}
                          onClick={() => selectCatalogEntry(entry)}
                          onMouseEnter={() => setCatalogActive(index)}
                        >
                          <span className="flex items-center justify-between gap-2">
                            <span className="min-w-0">
                              <span className="font-mono">
                                {entry.providerId} / {entry.modelId}
                              </span>
                              {entry.releaseDate ? (
                                <span className="text-muted-foreground ml-2">
                                  {t("usage.billing.pricingCatalogReleaseDate", {
                                    date: entry.releaseDate,
                                  })}
                                </span>
                              ) : null}
                              {entry.conditional ? (
                                <span className="text-muted-foreground ml-2 rounded border px-1 text-[10px]">
                                  {t("usage.billing.pricingCatalogConditional")}
                                </span>
                              ) : null}
                              {!entry.hasRates ? (
                                <span className="text-muted-foreground ml-2 rounded border px-1 text-[10px]">
                                  {t("usage.billing.pricingCatalogNoRates")}
                                </span>
                              ) : null}
                            </span>
                            {/* 复制完整 provider/model；失败静默（剪贴板权限在部分环境不可用） */}
                            <Button
                              type="button"
                              size="icon-xs"
                              variant="ghost"
                              className="text-muted-foreground shrink-0"
                              aria-label={t("common.copy")}
                              title={t("common.copy")}
                              data-testid={`pricing-catalog-copy-${entry.providerId}/${entry.modelId}`}
                              onMouseDown={(event) => event.preventDefault()}
                              onClick={(event) => {
                                event.stopPropagation();
                                void Promise.resolve()
                                  .then(() =>
                                    navigator.clipboard?.writeText(
                                      `${entry.providerId}/${entry.modelId}`
                                    )
                                  )
                                  .catch(() => {});
                              }}
                            >
                              <Copy aria-hidden="true" />
                            </Button>
                          </span>
                        </li>
                      ))}
                    </ul>
                  ) : catalogOpen && catalogResults.length === 0 ? (
                    <p
                      data-testid="pricing-catalog-empty"
                      className="text-muted-foreground text-[11px]"
                    >
                      {t("usage.billing.pricingCatalogEmpty")}
                    </p>
                  ) : null}
                </div>

                <div className="flex flex-wrap items-center gap-2 text-xs">
                  <Input
                    data-testid="pricing-provider-id"
                    className="h-8 w-40"
                    placeholder={t("usage.billing.pricingProviderId")}
                    value={pricingProviderId}
                    onChange={(e) => setPricingProviderId(e.target.value)}
                  />
                  <Input
                    data-testid="pricing-model-id"
                    className="h-8 w-48"
                    placeholder={t("usage.billing.pricingModelId")}
                    value={pricingModelId}
                    onChange={(e) => setPricingModelId(e.target.value)}
                  />
                  <span
                    data-testid="pricing-target-preview"
                    className="text-muted-foreground font-mono"
                  >
                    {pricingProviderId.trim() || EM_DASH} / {pricingModelId.trim() || EM_DASH}
                  </span>
                </div>
                <label className="flex items-center gap-2 text-xs">
                  <input
                    type="checkbox"
                    data-testid="pricing-authorize"
                    checked={pricingAuthorized}
                    onChange={(e) => setPricingAuthorized(e.target.checked)}
                  />
                  {t("usage.billing.pricingAuthorize")}
                </label>
                <p className="text-muted-foreground text-[11px]">
                  {t("usage.billing.pricingExactHint")}
                </p>
                {pricingError ? (
                  <p data-testid="pricing-error" className="text-destructive text-xs" role="alert">
                    {usageErrMsg(pricingError, t)}
                    {" · "}
                    {pricingHintText(pricingError, t)}
                  </p>
                ) : null}
                <Button
                  size="sm"
                  data-testid="pricing-save"
                  disabled={busy || !pricingReady}
                  onClick={() => void savePricingRule()}
                >
                  {t("usage.billing.pricingSave")}
                </Button>
              </div>
            </section>

            {/* 冲突 / 含糊 */}
            <section className="space-y-1">
              <h3 className="text-muted-foreground text-xs font-medium">
                {t("usage.billing.issues")}
              </h3>
              {issues.conflicts.length === 0 && issues.ambiguous.length === 0 ? (
                <p className="text-muted-foreground text-xs">{t("usage.billing.noIssues")}</p>
              ) : (
                <ul className="space-y-1 text-xs">
                  {issues.conflicts.map((label) => (
                    <li key={`c-${label}`} className="text-amber-600 dark:text-amber-500">
                      {t("usage.billing.conflict", { identity: label })}
                    </li>
                  ))}
                  {issues.ambiguous.map((label) => (
                    <li key={`a-${label}`} className="text-amber-600 dark:text-amber-500">
                      {t("usage.billing.ambiguous", { identity: label })}
                    </li>
                  ))}
                </ul>
              )}
            </section>

            {/* 撤销 */}
            <section className="flex flex-wrap items-end gap-2">
              <label className="text-muted-foreground text-xs">
                {t("usage.billing.undoTarget")}
                <Input
                  data-testid="billing-undo-target"
                  className="mt-1 h-8 w-24"
                  inputMode="numeric"
                  value={undoTarget}
                  onChange={(e) => setUndoTarget(e.target.value.replace(/\D/g, ""))}
                />
              </label>
              <Button
                size="sm"
                variant="outline"
                disabled={busy}
                data-testid="billing-undo"
                onClick={() => void handleUndo()}
              >
                {t("usage.billing.undo")}
              </Button>
            </section>
          </div>
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

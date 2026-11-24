// DSH 桌面端远控管理卡（Task 4 重构：单开关「远程控制 DSH」拥有完整生命周期）。
//
// 本组件是**容器**：展开/折叠、实例发现与选择、自动适配开关；开关状态机、
// 线性推进、就绪结果与修复入口全部在 [`DshRemoteSwitchPanel`]（子组件，消费
// 后端持久状态机）。
//
// - 状态/作业 = `src/lib/query/queries/dsh.ts`（键 `["dsh", …]`；可见性轮询）；
// - 自动适配开关 = `dsh_remote_status.autoAdaptEnabled`（缺省开）+
//   `dsh_set_auto_adapt`（后端唯一真值，前端回读不假同步）；
// - 单实例可直接展示明确目标（展开时自动登记选择唯一实例）；多实例要求用户
//   先选择（DSH Spec §6.1）；
// - 常规界面没有独立的安装/绑定/撤权/卸载按钮（R2 开关门：公开 IPC 也不允许
//   绕过开关激活远控）；唯一保留的管理入口 = 可展开的「修复插件」；
// - 移动端无本卡入口。
import { useEffect, useMemo, useRef, useState } from "react";
import { QueryClientProvider, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { formatInvokeError } from "@/lib/invokeError";
import { queryClient } from "@/lib/query/queryClient";
import { ToolConfigCard } from "@/components/settings/ToolConfigCard";
import { DshRemoteSwitchPanel } from "@/components/settings/DshRemoteSwitchPanel";
import { dshInstanceSelect, dshSetAutoAdapt } from "@/lib/api/dsh";
import {
  DSH_INSTANCES_KEY,
  DSH_KEY,
  DSH_MANAGEMENT_KEY,
  useDshInstancesQuery,
  useDshManagementQuery,
  useDocumentVisible,
} from "@/lib/query/queries/dsh";

function DshRemoteSectionBody() {
  const { t } = useAppTranslation();
  const qc = useQueryClient();
  const visible = useDocumentVisible();
  // 折叠态由外层 ToolConfigCard 持有：它只在展开时挂载本组件，因此**挂载即展开**
  // ——收起时本组件不执行，实例发现/选择/作业查询全部为零（验收要求）。
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const instancesQ = useDshInstancesQuery(visible);
  const instances = useMemo(() => instancesQ.data ?? [], [instancesQ.data]);
  const savedSelectedId = instances.find((instance) => instance.selected)?.instanceId ?? null;
  const targetId = selectedId ?? savedSelectedId;
  const targetName =
    instances.find((instance) => instance.instanceId === targetId)?.displayName ?? null;
  // 自动适配开关（后端唯一真值：legacy 管理状态叠加面）。
  const legacyStatusQ = useDshManagementQuery(visible);
  const autoAdaptEnabled = legacyStatusQ.data?.autoAdaptEnabled ?? true;
  const [actionError, setActionError] = useState<string | null>(null);
  const [mutating, setMutating] = useState(false);
  // 单实例自动选择（一次；多实例仍要求用户明确选择）。
  const autoSelected = useRef(false);

  useEffect(() => {
    if (autoSelected.current || mutating || instances.length !== 1 || savedSelectedId !== null)
      return;
    autoSelected.current = true;
    void (async () => {
      try {
        await dshInstanceSelect(instances[0].instanceId);
        setSelectedId(instances[0].instanceId);
        await qc.invalidateQueries({ queryKey: DSH_INSTANCES_KEY });
        await qc.invalidateQueries({ queryKey: DSH_MANAGEMENT_KEY });
      } catch (e) {
        setActionError(formatInvokeError(e, t));
      }
    })();
  }, [instances, savedSelectedId, mutating, qc, t]);

  const selectInstance = async (instanceId: string) => {
    if (mutating) return;
    setMutating(true);
    setActionError(null);
    try {
      await dshInstanceSelect(instanceId);
      setSelectedId(instanceId);
      await qc.invalidateQueries({ queryKey: DSH_INSTANCES_KEY });
      await qc.invalidateQueries({ queryKey: DSH_MANAGEMENT_KEY });
      await qc.invalidateQueries({ queryKey: [...DSH_KEY, "switch"] });
    } catch (e) {
      setActionError(formatInvokeError(e, t));
    } finally {
      setMutating(false);
    }
  };

  const toggleAutoAdapt = async (value: boolean) => {
    setMutating(true);
    setActionError(null);
    try {
      await dshSetAutoAdapt(value);
      await qc.invalidateQueries({ queryKey: DSH_MANAGEMENT_KEY });
    } catch (e) {
      setActionError(formatInvokeError(e, t));
    } finally {
      setMutating(false);
    }
  };

  return (
    <>
      {/* 多于一个条目、或有任何一条不在运行、或尚无选择时显示选择器：
          「按提示退出 DSH 之后」重开与切换实例的入口不能消失。 */}
      {(instances.length > 1 ||
        instances.some((candidate) => !candidate.live) ||
        (instances.length >= 1 && targetId === null)) && (
        <div data-testid="dsh-instance-picker" className="mt-2 space-y-1.5 rounded-lg border p-3">
          <p className="text-muted-foreground text-xs">{t("settings.remote.dshInstancePick")}</p>
          <ul className="space-y-1">
            {instances.map((candidate) => (
              <li key={candidate.instanceId} className="flex items-center justify-between gap-2">
                <span className="min-w-0 flex-1 truncate text-xs">
                  {candidate.displayName} · {candidate.kind}
                  {!candidate.live && (
                    <span
                      data-testid={`dsh-instance-offline-${candidate.instanceId}`}
                      className="text-muted-foreground ml-1"
                    >
                      · {t("settings.remote.dshInstanceOffline")}
                    </span>
                  )}
                </span>
                <Button
                  size="sm"
                  variant={candidate.instanceId === targetId ? "default" : "outline"}
                  className="h-6 px-2 text-xs"
                  data-testid={`dsh-instance-${candidate.instanceId}`}
                  disabled={mutating}
                  onClick={() => void selectInstance(candidate.instanceId)}
                >
                  {candidate.instanceId === targetId
                    ? t("settings.remote.dshInstanceSelected")
                    : t("settings.remote.dshInstanceSelect")}
                </Button>
              </li>
            ))}
          </ul>
        </div>
      )}

      <DshRemoteSwitchPanel instanceId={targetId} visible={visible} displayName={targetName} />

      {/* 自动适配与错误：与开关面板同属卡内正文 */}
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.dshAutoAdapt")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.dshAutoAdaptHint")}
          </p>
        </div>
        <Switch
          data-testid="dsh-auto-adapt-switch"
          aria-label={t("settings.remote.dshAutoAdapt")}
          checked={autoAdaptEnabled}
          disabled={mutating || legacyStatusQ.isLoading}
          onCheckedChange={(v) => void toggleAutoAdapt(v)}
        />
      </div>

      {actionError !== null && (
        <p data-testid="dsh-error" className="mt-1 text-xs text-rose-600 dark:text-rose-400">
          {actionError}
        </p>
      )}
    </>
  );
}

/** 挂载壳：与 Codex APP / ZCode 同款工具卡（共用卡头图标、键盘与无障碍语义）。
 *  卡持有展开态；正文只在展开时挂载——收起时零发现、零选择、零作业副作用。 */
export function DshRemoteSection() {
  const { t } = useAppTranslation();
  return (
    <QueryClientProvider client={queryClient}>
      <ToolConfigCard toolId="dsh" title={t("settings.remote.dshGroup")}>
        <DshRemoteSectionBody />
      </ToolConfigCard>
    </QueryClientProvider>
  );
}

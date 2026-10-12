// 预设组 v2 列表（M2 Phase B）：双分区（通用 / 工具私有）+ 预设卡片 + 预设×工具开关
// 数据层走 React Query（usePresetsQuery / useActivePresetsQuery），删除成功后 invalidate PRESETS_KEY；
// 开关契约（spec §5.5）：开 = 只开 T5 确认弹窗（apply 延迟到 onConfirm）；关 = 直接 restore + toast。
// 创建 / 编辑弹窗（T7 PresetEditDialog）：创建按钮开空表单，卡片点击回填该预设；
// 弹窗保存成功后自 invalidate PRESETS_KEY（本组件无需关心）。
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { formatInvokeError } from "@/lib/invokeError";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { ApplyConfirmDialog } from "@/components/presets/ApplyConfirmDialog";
import { PresetEditDialog } from "@/components/presets/PresetEditDialog";
import { PresetPreviewDialog } from "@/components/presets/PresetPreviewDialog";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Layers, Plus, Trash2, Play, X, Eye, Pencil } from "lucide-react";
import type { PresetRecord } from "@/types/preset";
import type { ActivePreset, ExtensionWithAssignments } from "@/types/extension";
import { ToolIcon } from "@/components/common/ToolIcon";
// review F4：工具列改后端下发（勾选状态驱动），停用工具不再出现在预设组选择中
import { useEnabledToolsQuery, type EnabledTool } from "@/lib/query/queries/tools";
import {
  ACTIVE_PRESETS_KEY,
  PRESETS_KEY,
  useActivePresetsQuery,
  usePresetsQuery,
} from "@/lib/query/queries/presets";
import {
  applyPreset,
  applyPresetToSubagent,
  deactivatePresetFromSubagent,
  deletePreset,
  restorePreset,
} from "@/lib/api/preset";

export function PresetList({
  // extensions 透传给 T7 编辑弹窗作套件列表数据源（props 契约不变）
  extensions,
}: {
  extensions: ExtensionWithAssignments[];
}) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const { data: enabledTools = [] } = useEnabledToolsQuery();
  const { data: presets = [] } = usePresetsQuery();
  // 工具当前激活的预设（开关 checked 状态源）：apply/restore 后 invalidate 即自动翻转
  const { data: activePresets = [] } = useActivePresetsQuery();
  // 删除确认弹窗目标（null = 关闭）
  const [deleteTarget, setDeleteTarget] = useState<PresetRecord | null>(null);
  // 编辑弹窗（T7）：open + 编辑目标（null = 新建空表单）
  const [editOpen, setEditOpen] = useState(false);
  const [editPreset, setEditPreset] = useState<PresetRecord | null>(null);
  // 只读预览弹窗目标（UI 改版 B5：卡片 👁 入口；null = 关闭）
  const [previewTarget, setPreviewTarget] = useState<PresetRecord | null>(null);
  // 应用确认弹窗目标（T5 ApplyConfirmDialog；null = 关闭）。toolName 预查好供弹窗标题
  const [confirmTarget, setConfirmTarget] = useState<{
    presetId: string;
    toolId: string;
    toolName: string;
  } | null>(null);

  // 通用区：scope === "universal"
  const universalPresets = presets.filter((p) => p.scope === "universal");

  // 私有区：按 boundTool 分组，组顺序跟随启用工具列（停用/未知工具按首次出现排后）
  const byTool = new Map<string, PresetRecord[]>();
  for (const p of presets) {
    if (p.scope !== "tool") continue;
    const key = p.boundTool ?? "";
    const list = byTool.get(key);
    if (list) list.push(p);
    else byTool.set(key, [p]);
  }
  const toolGroups: { toolId: string; label: string; presets: PresetRecord[] }[] = [];
  const rest = new Map(byTool);
  for (const tool of enabledTools) {
    const list = rest.get(tool.id);
    if (list) {
      toolGroups.push({ toolId: tool.id, label: tool.label, presets: list });
      rest.delete(tool.id);
    }
  }
  for (const [toolId, list] of rest) {
    // boundTool 缺失的脏数据回退分区名兜底，避免渲染空组头
    toolGroups.push({ toolId, label: toolId || t("presets.typeTool"), presets: list });
  }

  // 创建：打开编辑弹窗空表单（preset = null）
  const handleCreate = () => {
    setEditPreset(null);
    setEditOpen(true);
  };

  // 编辑：卡片点击入口（T7）——回填该预设打开弹窗
  const openEdit = (preset: PresetRecord) => {
    setEditPreset(preset);
    setEditOpen(true);
  };

  // 开关契约（spec §5.5）：开 → 只开确认弹窗（apply 延迟到弹窗 onConfirm）；关 → 直接恢复默认。
  // 两个分支都自捕获错误（T5 carry-note：不得向外抛未捕获 rejection）
  const onSwitch = async (presetId: string, toolId: string, next: boolean) => {
    if (next) {
      setConfirmTarget({
        presetId,
        toolId,
        toolName: enabledTools.find((tool) => tool.id === toolId)?.label ?? toolId,
      });
      return;
    }
    try {
      const rr = await restorePreset(toolId);
      toast.success(
        t("presets.restoreDone", { m: rr.restoredMam.length, n: rr.restoredNative.length })
      );
      if (rr.conflicts.length)
        toast.warning(t("presets.restoreConflicts"), {
          description: rr.conflicts.join("\n"),
        });
      await qc.invalidateQueries({ queryKey: ACTIVE_PRESETS_KEY });
      // 托盘同步（Task 16）：选中态变化后重建托盘菜单；失败静默，不影响主流程
      invoke("refresh_tray", { presetsLabel: t("tray.presetsLabel") }).catch(() => {});
    } catch (e) {
      // 终审 Minor #1：restore（开关关）失败用专用文案，不与 apply 分支的「应用失败」混用
      toast.error(t("presets.restoreFailed", { error: formatInvokeError(e, t) }));
    }
  };

  // 弹窗确认 → 真正 apply（三计数 toast）→ invalidate 后关弹窗；失败 toast 并留在弹窗可重试。
  // 必须自捕获（catch 而非 finally 抛出）：ApplyConfirmDialog 内部 await onConfirm，外抛即未处理 rejection
  const confirmApply = async () => {
    if (!confirmTarget) return;
    try {
      const r = await applyPreset(confirmTarget.presetId, confirmTarget.toolId);
      toast.success(
        t("presets.applyResult", {
          n: r.successCount,
          d: r.disabled.length,
          s: r.stashed.length,
        })
      );
      await qc.invalidateQueries({ queryKey: ACTIVE_PRESETS_KEY });
      // 托盘同步（Task 16）：同上，开（应用）成功后重建托盘选中态
      invoke("refresh_tray", { presetsLabel: t("tray.presetsLabel") }).catch(() => {});
      setConfirmTarget(null);
    } catch (e) {
      toast.error(t("presets.applyFailed", { error: formatInvokeError(e, t) }));
    }
  };

  const confirmDelete = async () => {
    if (!deleteTarget) return;
    try {
      await deletePreset(deleteTarget.id);
      toast.success(t("common.deleted"));
      qc.invalidateQueries({ queryKey: PRESETS_KEY });
      // 托盘同步（Task 16）：预设删除后移除对应托盘项
      invoke("refresh_tray", { presetsLabel: t("tray.presetsLabel") }).catch(() => {});
    } catch (e) {
      // 后端守卫：预设正在应用中时错误串带 PRESET_ACTIVE 标记（commands/preset.rs）
      const msg = formatInvokeError(e, t);
      if (msg.includes("PRESET_ACTIVE")) toast.error(t("presets.cannotDeleteActive"));
      else toast.error(t("common.deleteFailed", { error: msg }));
    } finally {
      setDeleteTarget(null);
    }
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <h3 className="flex items-center gap-2 text-sm font-semibold">
          <Layers className="h-4 w-4" />
          {t("presets.titleCount", { n: presets.length })}
        </h3>
        <Button size="sm" variant="outline" onClick={handleCreate}>
          <Plus className="mr-1.5 h-3.5 w-3.5" />
          {t("common.create")}
        </Button>
      </div>

      {presets.length === 0 ? (
        <p className="text-muted-foreground py-2 text-xs">{t("presets.empty")}</p>
      ) : (
        <div className="space-y-3">
          {/* 通用区：无通用预设时整区隐藏 */}
          {universalPresets.length > 0 && (
            <section className="space-y-1">
              <h4 className="text-muted-foreground text-xs font-medium">
                {t("presets.universalSection")}
              </h4>
              {universalPresets.map((preset) => (
                <PresetCard
                  key={preset.id}
                  preset={preset}
                  enabledTools={enabledTools}
                  activePresets={activePresets}
                  onSwitch={onSwitch}
                  onRequestDelete={setDeleteTarget}
                  onEdit={openEdit}
                  onPreview={setPreviewTarget}
                />
              ))}
            </section>
          )}
          {/* 私有区：按绑定工具分组（组头 = 工具名），无私有预设时整区隐藏 */}
          {toolGroups.length > 0 && (
            <section className="space-y-1">
              <h4 className="text-muted-foreground text-xs font-medium">
                {t("presets.toolScopedSection")}
              </h4>
              {toolGroups.map((group) => (
                <div key={group.toolId} className="space-y-1">
                  <div className="flex items-center gap-1.5 pl-1 text-xs font-medium">
                    <ToolIcon toolId={group.toolId} size={14} />
                    {group.label}
                  </div>
                  {group.presets.map((preset) => (
                    <PresetCard
                      key={preset.id}
                      preset={preset}
                      enabledTools={enabledTools}
                      activePresets={activePresets}
                      onSwitch={onSwitch}
                      onRequestDelete={setDeleteTarget}
                      onEdit={openEdit}
                      onPreview={setPreviewTarget}
                    />
                  ))}
                </div>
              ))}
            </section>
          )}
        </div>
      )}

      {/* 删除确认弹窗：标题即预设名，取消 / 破坏性确定 */}
      <Dialog
        open={deleteTarget !== null}
        onOpenChange={(open) => {
          if (!open) setDeleteTarget(null);
        }}
      >
        <DialogContent aria-describedby={undefined} className="max-w-sm">
          <DialogHeader>
            <DialogTitle className="truncate pr-4">{deleteTarget?.name}</DialogTitle>
          </DialogHeader>
          <DialogFooter>
            <Button size="sm" variant="outline" onClick={() => setDeleteTarget(null)}>
              {t("common.cancel")}
            </Button>
            <Button size="sm" variant="destructive" onClick={confirmDelete}>
              {t("common.confirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 应用确认弹窗（T5）：开开关 → 预览清单 → 确认后经 confirmApply 真正 apply */}
      <ApplyConfirmDialog
        open={confirmTarget !== null}
        presetId={confirmTarget?.presetId ?? ""}
        toolId={confirmTarget?.toolId ?? ""}
        toolName={confirmTarget?.toolName ?? ""}
        onClose={() => setConfirmTarget(null)}
        onConfirm={confirmApply}
      />

      {/* 只读预览弹窗（UI 改版 B5）：预设名册来自 ExtensionList 透传的 extensions */}
      <PresetPreviewDialog
        preset={previewTarget}
        extensions={extensions}
        open={previewTarget !== null}
        onOpenChange={(o) => {
          if (!o) setPreviewTarget(null);
        }}
      />

      {/* 编辑弹窗（T7，新建/编辑共用）：保存成功后弹窗自 invalidate PRESETS_KEY */}
      <PresetEditDialog
        open={editOpen}
        preset={editPreset}
        presetExtensions={extensions}
        onClose={() => setEditOpen(false)}
      />
    </div>
  );
}

/** 预设卡片：名称 + 描述摘要（1 行截断）+ items 计数徽标 + 预设×工具开关 + 删除入口；整卡可点（onEdit → T7 编辑弹窗） */
function PresetCard({
  preset,
  enabledTools,
  activePresets,
  onSwitch,
  onRequestDelete,
  onEdit,
  onPreview,
}: {
  preset: PresetRecord;
  enabledTools: EnabledTool[];
  activePresets: ActivePreset[];
  onSwitch: (presetId: string, toolId: string, next: boolean) => void;
  onRequestDelete: (preset: PresetRecord) => void;
  onEdit: (preset: PresetRecord) => void;
  onPreview: (preset: PresetRecord) => void;
}) {
  const { t } = useTranslation();

  // 资源能力门（照抄 ResourceByKindView 的 kindSupported 判定，评审裁决不抽公共模块）：
  // 工具 × 资源类型是否支持启停（后端 EnabledTool 标志下发）
  const kindSupported = (tool: EnabledTool, kind: string): boolean =>
    kind === "skill"
      ? tool.skillToggleSupported
      : kind === "mcp"
        ? tool.mcpSupported
        : tool.pluginSupported;

  // 开关目标工具（spec §7.1）：私有预设 = 绑定工具一枚（绑定工具未启用 → 不渲染开关）；
  // 通用预设 = 每个已启用工具一枚
  const switchTools: EnabledTool[] =
    preset.scope === "tool" && preset.boundTool
      ? enabledTools.filter((tool) => tool.id === preset.boundTool)
      : enabledTools;

  return (
    <div
      className="hover:bg-accent/30 cursor-pointer rounded border p-2 transition-colors"
      onClick={() => onEdit(preset)}
    >
      <div className="flex items-center justify-between gap-2">
        <span className="truncate text-sm font-medium">{preset.name}</span>
        <div className="flex shrink-0 items-center gap-1.5">
          {/* items 计数徽标 */}
          <span className="bg-muted text-muted-foreground rounded px-1.5 py-0.5 text-[10px]">
            {preset.items.length}
          </span>
          {/* 👁 只读预览（UI 改版 B5）：按 Skill/MCP/插件 分组 + 来源徽章 */}
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onPreview(preset);
            }}
            className="text-muted-foreground hover:text-foreground"
            title={t("presets.previewPreset")}
            aria-label={t("presets.previewPreset")}
          >
            <Eye className="h-3.5 w-3.5" />
          </button>
          {/* ✏️ 编辑（UI 改版 B5）：与整卡点击同一入口（onEdit 预填打开） */}
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onEdit(preset);
            }}
            className="text-muted-foreground hover:text-foreground"
            title={t("presets.editPreset")}
            aria-label={t("presets.editPreset")}
          >
            <Pencil className="h-3.5 w-3.5" />
          </button>
          <button
            onClick={(e) => {
              // 不触发整卡点击（onEdit 编辑弹窗）
              e.stopPropagation();
              onRequestDelete(preset);
            }}
            className="text-muted-foreground hover:text-red-500"
          >
            <Trash2 className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>
      {/* 描述摘要：1 行截断 */}
      {preset.description && (
        <p className="text-muted-foreground mt-0.5 truncate text-[11px]">{preset.description}</p>
      )}
      {/* 预设×工具开关：checked 由 list_active_presets 下发；items 含工具不支持的资源类型 → disabled + title
          （控制器裁决：门控只挡「开」不挡「关」——已激活开关保持可操作以走 restore 恢复路径，spec §5.5/§7.3）。
          整行阻断冒泡，避免误触整卡点击（onEdit 编辑弹窗） */}
      {switchTools.length > 0 && (
        <div
          className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1"
          onClick={(e) => e.stopPropagation()}
        >
          {switchTools.map((tool) => {
            const checked = activePresets.find((a) => a.toolId === tool.id)?.presetId === preset.id;
            const gated = preset.items.some((item) => !kindSupported(tool, item.kind));
            // disabled 仅作用于「未激活 + 门控」组合；已激活开关恒可关（关 = 恢复默认，安全动作）
            const disabled = gated && !checked;
            return (
              <span
                key={tool.id}
                className="flex items-center gap-1 text-xs"
                title={disabled ? `${tool.label}: ${t("resources.kindNotSupported")}` : undefined}
              >
                <ToolIcon toolId={tool.id} size={12} />
                <span className="text-muted-foreground">{tool.label}</span>
                <Switch
                  checked={checked}
                  disabled={disabled}
                  onCheckedChange={(next) => onSwitch(preset.id, tool.id, next)}
                />
              </span>
            );
          })}
        </div>
      )}
      {/* 子 Agent 级操作（遗留特性，原样保留迁移）：私有预设挂绑定工具，通用预设沿用旧的逐启用工具形态 */}
      {preset.scope === "tool" && preset.boundTool ? (
        <SubAgentPresetActions
          presetId={preset.id}
          presetName={preset.name}
          toolId={preset.boundTool}
        />
      ) : (
        enabledTools.map((tool) => (
          <SubAgentPresetActions
            key={tool.id}
            presetId={preset.id}
            presetName={preset.name}
            toolId={tool.id}
          />
        ))
      )}
    </div>
  );
}

function SubAgentPresetActions({
  presetId,
  presetName,
  toolId,
}: {
  presetId: string;
  presetName: string;
  toolId: string;
}) {
  const { t } = useTranslation();
  const [subAgents, setSubAgents] = useState<string[]>([]);
  const [expanded, setExpanded] = useState(false);

  const loadSubAgents = async () => {
    try {
      const data = await invoke<string[]>("detect_subagents", { toolId });
      setSubAgents(data);
    } catch (e) {
      console.error("Failed to load subagents:", e);
    }
  };

  const handleApplyToSubagent = async (subAgentId: string) => {
    try {
      const result = await applyPresetToSubagent(presetId, toolId, subAgentId);
      if (result.failures.length > 0) {
        toast.warning(
          t("presets.partialSuccess", { n: result.successCount, failed: result.failures.length })
        );
      } else {
        toast.success(
          t("presets.appliedToSubagent", { name: presetName, tool: toolId, subAgent: subAgentId })
        );
      }
    } catch (e) {
      toast.error(t("presets.applyFailed", { error: formatInvokeError(e, t) }));
    }
  };

  const handleDeactivateFromSubagent = async (subAgentId: string) => {
    try {
      await deactivatePresetFromSubagent(presetId, toolId, subAgentId);
      toast.success(
        t("presets.deactivatedFromSubagent", {
          name: presetName,
          tool: toolId,
          subAgent: subAgentId,
        })
      );
    } catch (e) {
      toast.error(t("presets.deactivateFailed", { error: formatInvokeError(e, t) }));
    }
  };

  if (subAgents.length === 0) return null;

  return (
    <div>
      <button
        onClick={() => {
          setExpanded(!expanded);
          if (!expanded) loadSubAgents();
        }}
        className="text-muted-foreground hover:text-foreground text-[10px]"
      >
        {expanded ? t("presets.collapseSubAgents") : t("presets.expandSubAgents")}
      </button>
      {expanded && (
        <div className="ml-2 space-y-0.5 border-l pl-2">
          {subAgents.map((sa) => (
            <div key={sa} className="flex items-center gap-1 text-[10px]">
              <span className="text-muted-foreground">{sa}</span>
              <Button
                size="sm"
                variant="ghost"
                className="h-4 px-1 text-[9px]"
                onClick={() => handleApplyToSubagent(sa)}
              >
                <Play className="mr-0.5 h-2 w-2" />
                {t("presets.apply")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-4 w-4 p-0 text-[9px]"
                onClick={() => handleDeactivateFromSubagent(sa)}
              >
                <X className="h-2 w-2" />
              </Button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

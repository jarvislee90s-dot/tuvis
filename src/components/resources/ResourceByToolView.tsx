import { useState, useCallback, useEffect } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { formatInvokeError } from "@/lib/invokeError";
import { Button } from "@/components/ui/button";
import {
  Scan,
  Import,
  FolderOpen,
  BookmarkPlus,
  ArrowUpDown,
  ArrowUp,
  ArrowDown,
} from "lucide-react";
import { ToolIcon } from "@/components/common/ToolIcon";
import { PresetEditDialog } from "@/components/presets/PresetEditDialog";
import { applyNameSort, cycleSortDir, type SortDir } from "@/lib/nameSort";
import {
  detectDuplicateSkills,
  cleanupDuplicateSkills,
  listExtensionsWithAssignments,
} from "@/lib/api/resource";
import { getToolActiveResources } from "@/lib/api/preset";
import { useEnabledToolsQuery } from "@/lib/query/queries/tools";
import type {
  ExtensionWithAssignments,
  FrontmatterSuggestion,
  NativeExtension,
  ToolResources,
  ImportStats,
} from "@/types/extension";
import { ImportSuggestionDialog } from "@/components/resources/ImportSuggestionDialog";
import { ResourceSourceBadge, type ResourceSource } from "./ResourceSourceBadge";

function formatSkillName(name: string): string {
  return name.includes("/") ? name.replace("/", ": ") : name;
}

/** .agents 遗留链接面的适用工具（UI 改版 C9）：后端 detect_legacy_agents_links 只认指向
 *  ~/.tuvis/active/codex（Layer 2）的链接（migration.rs is_legacy_codex_link），
 *  故「.agents 可用」徽章仅 codex 卡面会出现 */
const LEGACY_AGENTS_TOOL = "codex";

export function ResourceByToolView() {
  const { t } = useTranslation();
  // 工具列由后端下发（勾选状态驱动，W5）
  const { data: tools = [] } = useEnabledToolsQuery();
  const [toolResources, setToolResources] = useState<Record<string, ToolResources>>({});
  const [scanning, setScanning] = useState<Record<string, boolean>>({});

  // FR-24「存为预设」：弹窗开关 + 预填 + 套件列表数据源。
  // prefill 必须持有在 state（T7 carry-note：弹窗 open-effect 依赖 prefill 身份，
  // JSX 内联字面量每次渲染都是新对象，会在编辑中途重置表单）
  const [presetDlgOpen, setPresetDlgOpen] = useState(false);
  const [presetPrefill, setPresetPrefill] = useState<{
    toolId: string;
    items: [string, string][];
  } | null>(null);
  const [presetExtensions, setPresetExtensions] = useState<ExtensionWithAssignments[]>([]);
  const [prefilling, setPrefilling] = useState<Record<string, boolean>>({});
  // frontmatter 专属预填建议（spec §6，Task 17）：单项目导入命中后弹确认 Dialog
  //（2026-09-15 裁决：当场确认，非 toast）
  const [suggestion, setSuggestion] = useState<FrontmatterSuggestion | null>(null);
  // 名称筛选 + 三态排序（用户反馈：工具组下清单可能很长）——与「按资源」视图同款口径，
  // 排序/筛选逻辑复用 src/lib/nameSort.ts；作用于每个工具组内的全部清单
  const [search, setSearch] = useState("");
  const [sortDir, setSortDir] = useState<SortDir>("none");

  const handleSaveAsPreset = async (toolId: string) => {
    setPrefilling((prev) => ({ ...prev, [toolId]: true }));
    try {
      // 双拉取：当前生效资源（三元组，origin 丢弃——预设条目即二元组）+ 全量套件列表。
      // 本视图已有的 list_tool_resources 数据缺 isNative 字段，不能直接作弹窗数据源
      //（原生技能分组会失真），故补一次 list_extensions_with_assignments 加载
      const [triples, extensions] = await Promise.all([
        getToolActiveResources(toolId),
        listExtensionsWithAssignments(),
      ]);
      setPresetExtensions(extensions as ExtensionWithAssignments[]);
      setPresetPrefill({
        toolId,
        items: triples.map(([id, kind]) => [id, kind] as [string, string]),
      });
      setPresetDlgOpen(true);
    } catch (e) {
      toast.error(t("common.operationFailed", { error: formatInvokeError(e, t) }));
    } finally {
      setPrefilling((prev) => ({ ...prev, [toolId]: false }));
    }
  };

  const loadToolResources = useCallback(async (toolId: string) => {
    try {
      const data = await invoke<ToolResources>("list_tool_resources", { toolId });
      setToolResources((prev) => ({ ...prev, [toolId]: data }));
    } catch (e) {
      console.error(`Failed to load resources for ${toolId}:`, e);
    }
  }, []);

  // 挂载时（及工具勾选变化后）自动加载所有启用工具的已有全局资源
  useEffect(() => {
    tools.forEach((tool) => {
      loadToolResources(tool.id);
    });
  }, [tools, loadToolResources]);

  const handleScan = async (toolId: string) => {
    setScanning((prev) => ({ ...prev, [toolId]: true }));
    try {
      const native = await invoke<NativeExtension[]>("scan_native_resources", { toolId });
      if (native.length > 0) {
        toast.info(
          t("resources.foundNative", {
            tool: tools.find((tool) => tool.id === toolId)?.label,
            n: native.length,
          })
        );
      } else {
        toast.info(t("resources.noNewNative"));
      }
      await loadToolResources(toolId);
    } catch (e) {
      toast.error(t("common.scanFailed", { error: formatInvokeError(e, t) }));
    } finally {
      setScanning((prev) => ({ ...prev, [toolId]: false }));
    }
  };

  const handleImport = async (toolId: string, item: NativeExtension) => {
    try {
      const result = await invoke<ImportStats>("import_native_resources", {
        items: [[item.sourcePath, item.name, toolId]],
      });
      if (result.imported > 0) {
        toast.success(t("resources.importSuccess", { name: item.name }));
        if (result.suggestion) setSuggestion(result.suggestion);
        await loadToolResources(toolId);
        await loadDuplicates(toolId);
      } else {
        toast.info(t("resources.alreadyExists", { name: item.name }));
      }
    } catch (e) {
      toast.error(t("resources.importFailed", { error: formatInvokeError(e, t) }));
    }
  };

  const [duplicates, setDuplicates] = useState<Record<string, string[]>>({});

  // .agents 遗留链接名册（UI 改版 C9「.agents 可用」徽章数据源）：挂载时检测一次；
  // 失败静默降级为空名册（该徽章面自然消失，不影响其余来源）
  const [legacyAgents, setLegacyAgents] = useState<string[]>([]);
  useEffect(() => {
    invoke<string[]>("detect_legacy_agents_links")
      .then((names) => setLegacyAgents(names))
      .catch(() => {});
  }, []);

  const loadDuplicates = useCallback(async (toolId: string) => {
    try {
      const dups = await detectDuplicateSkills(toolId);
      setDuplicates((prev) => ({ ...prev, [toolId]: dups }));
    } catch (e) {
      console.error(`Failed to detect duplicates for ${toolId}:`, e);
    }
  }, []);

  // 挂载时（及工具勾选变化后）检测所有启用工具的重复
  useEffect(() => {
    tools.forEach((tool) => {
      loadDuplicates(tool.id);
    });
  }, [tools, loadDuplicates]);

  const handleCleanupSingle = async (toolId: string, name: string) => {
    try {
      await cleanupDuplicateSkills(toolId, [name]);
      toast.success(t("resources.cleanedOne", { name }));
      await loadDuplicates(toolId);
      await loadToolResources(toolId);
    } catch (e) {
      toast.error(t("resources.cleanupFailed", { error: formatInvokeError(e, t) }));
    }
  };

  const handleCleanupAll = async (toolId: string) => {
    const dups = duplicates[toolId] || [];
    if (dups.length === 0) return;
    try {
      await cleanupDuplicateSkills(toolId, dups);
      toast.success(t("resources.cleanedCount", { n: dups.length }));
      await loadDuplicates(toolId);
      await loadToolResources(toolId);
    } catch (e) {
      toast.error(t("resources.cleanupFailed", { error: formatInvokeError(e, t) }));
    }
  };

  const handleOpenDir = async (toolId: string) => {
    try {
      const path = await invoke<string>("open_tool_resource", { toolId, kind: "skill" });
      toast.success(path);
    } catch (e) {
      toast.error(t("common.operationFailed", { error: formatInvokeError(e, t) }));
    }
  };

  return (
    <div className="space-y-4">
      {/* 名称筛选 + 三态排序控制条：作用于下方每个工具组内的全部清单 */}
      <div className="flex items-center gap-2">
        <input
          type="text"
          placeholder={t("resources.filterByName")}
          value={search}
          onChange={(e) => setSearch(e.currentTarget.value)}
          className="h-7 w-40 rounded border px-2 text-xs"
        />
        <Button
          size="sm"
          variant="ghost"
          className={`h-6 px-1.5 text-[10px] ${sortDir !== "none" ? "text-foreground" : "text-muted-foreground"}`}
          title={t("resources.sortByName")}
          aria-label={t("resources.sortByName")}
          onClick={() => setSortDir((prev) => cycleSortDir(prev))}
        >
          {sortDir === "asc" ? (
            <ArrowUp className="mr-1 h-3 w-3" />
          ) : sortDir === "desc" ? (
            <ArrowDown className="mr-1 h-3 w-3" />
          ) : (
            <ArrowUpDown className="mr-1 h-3 w-3" />
          )}
          {t("resources.sortByName")}
          {sortDir !== "none" && <span>{sortDir === "asc" ? "↑" : "↓"}</span>}
        </Button>
      </div>
      {tools.map((tool) => (
        <div key={tool.id} className="rounded border p-3">
          <div className="mb-2 flex items-center justify-between">
            <h3 className="flex items-center gap-2 text-sm font-semibold">
              <ToolIcon toolId={tool.id} size={18} />
              {tool.label}
            </h3>
            <div className="flex gap-1">
              <Button
                size="sm"
                variant="ghost"
                className="h-6 px-2 text-[10px]"
                title={t("resources.openToolDir", { tool: tool.label, kind: "skills" })}
                onClick={() => handleOpenDir(tool.id)}
              >
                <FolderOpen className="mr-1 h-3 w-3" />
                {t("resources.openDir")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-6 px-2 text-[10px]"
                onClick={() => handleScan(tool.id)}
                disabled={scanning[tool.id]}
              >
                <Scan className={`mr-1 h-3 w-3 ${scanning[tool.id] ? "animate-spin" : ""}`} />
                {t("common.scan")}
              </Button>
              {/* FR-24「存为预设」入口：抓取该工具当前生效资源预填编辑弹窗 */}
              <Button
                size="sm"
                variant="ghost"
                className="h-6 px-2 text-[10px]"
                title={t("presets.saveAsPreset")}
                onClick={() => handleSaveAsPreset(tool.id)}
                disabled={prefilling[tool.id]}
              >
                <BookmarkPlus
                  className={`mr-1 h-3 w-3 ${prefilling[tool.id] ? "animate-spin" : ""}`}
                />
                {t("presets.saveAsPreset")}
              </Button>
            </div>
          </div>

          <ToolResourceList
            toolId={tool.id}
            resources={toolResources[tool.id]}
            onImport={handleImport}
            search={search}
            sortDir={sortDir}
            legacyNames={new Set(legacyAgents)}
          />

          {/* 重复 skill 清理区 */}
          {(duplicates[tool.id]?.length ?? 0) > 0 && (
            <div className="mt-2 rounded border border-orange-500/30 bg-orange-500/5 p-2">
              <div className="mb-1 flex items-center justify-between">
                <span className="text-xs font-medium text-orange-600">
                  {t("resources.duplicatesWarning", { n: duplicates[tool.id]!.length })}
                </span>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-5 px-1 text-[10px] text-orange-600"
                  onClick={() => handleCleanupAll(tool.id)}
                >
                  {t("common.cleanupAll")}
                </Button>
              </div>
              <div className="space-y-0.5">
                {duplicates[tool.id]!.map((name) => (
                  <div key={name} className="flex items-center justify-between text-xs">
                    <span className="text-muted-foreground">{formatSkillName(name)}</span>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-5 px-1 text-[10px]"
                      onClick={() => handleCleanupSingle(tool.id, name)}
                    >
                      {t("common.cleanup")}
                    </Button>
                  </div>
                ))}
              </div>
            </div>
          )}
        </div>
      ))}

      {/* FR-24「存为预设」弹窗（复用 T7 编辑弹窗，新建模式 + 预填）：
          保存成功后弹窗自失效 PRESETS_KEY，此处无需额外刷新逻辑 */}
      <PresetEditDialog
        open={presetDlgOpen}
        preset={null}
        presetExtensions={presetExtensions}
        prefill={presetPrefill ?? undefined}
        onClose={() => setPresetDlgOpen(false)}
      />

      {/* frontmatter 专属预填建议确认（Task 17）：确认=写绑定，忽略=关闭不写 */}
      <ImportSuggestionDialog suggestion={suggestion} onClose={() => setSuggestion(null)} />
    </div>
  );
}

/** 「按工具」视图清单（UI 改版 C8/C9）：每工具卡只显示实际配置的资源——
 *  global 数组只保留该工具 assignments 已启用（enabled）项；native 原生项保留；
 *  另纳 .agents 遗留链接可达项（仅 codex，见 LEGACY_AGENTS_TOOL）。Skill 段按名合并
 *  三态互斥行（优先级 原生 > 全局仓库 > .agents 可用），每行右侧为来源徽章列；
 *  MCP / 插件段仅已链接项，统一佩「全局仓库」徽章；段头计数随之如实。 */
function ToolResourceList({
  toolId,
  resources,
  onImport,
  search,
  sortDir,
  legacyNames,
}: {
  toolId: string;
  resources?: ToolResources;
  onImport: (toolId: string, item: NativeExtension) => void;
  search: string;
  sortDir: SortDir;
  legacyNames: Set<string>;
}) {
  const { t } = useTranslation();
  if (!resources) {
    return <div className="text-muted-foreground py-2 text-xs">{t("common.loading")}</div>;
  }

  // 名称筛选（大小写不敏感包含）+ 三态排序：作用于合并后的行（口径与「按资源」视图一致）
  const q = search.trim().toLowerCase();
  const match = (name: string) => !q || name.toLowerCase().includes(q);

  // 「已启用/已链接」判定（UI 改版 C10，resource.rs list_tool_resources 契约）：
  // assignments 按 toolId 过滤下发、每项至多一条；enabled = 账本启用位——disable 时
  // 后端同步置 enabled=0 + link_status=missing，故 enabled 即「实际配置」的充分判据
  const isLinked = (e: ExtensionWithAssignments) =>
    e.assignments.some((a) => a.agentToolId === toolId && a.enabled);
  const linkedGlobal = resources.global.filter(isLinked);
  // 仅 .agents 遗留链接可达的仓库技能（未入启用账本；detect 面 codex 专属）
  const legacyOnlyGlobal = resources.global.filter(
    (e) =>
      e.kind === "skill" && !isLinked(e) && toolId === LEGACY_AGENTS_TOOL && legacyNames.has(e.name)
  );

  // Skill 行按名合并（三态互斥优先级 原生 > 全局仓库 > .agents 可用）：
  // 同名技能同时原生存在与全局链接（重复 skill 场景）只出一行，佩高优先级徽章；
  // 重复清理由上方独立警示区负责，不在清单内重复列行
  const skillRows = new Map<
    string,
    { name: string; source: ResourceSource; native?: NativeExtension }
  >();
  for (const n of resources.native) {
    if (n.kind === "skill") skillRows.set(n.name, { name: n.name, source: "native", native: n });
  }
  for (const e of linkedGlobal) {
    if (e.kind === "skill" && !skillRows.has(e.name)) {
      skillRows.set(e.name, { name: e.name, source: "repo" });
    }
  }
  for (const e of legacyOnlyGlobal) {
    if (!skillRows.has(e.name)) skillRows.set(e.name, { name: e.name, source: "agents" });
  }
  const sortedSkillRows = applyNameSort(
    [...skillRows.values()].filter((r) => match(r.name)),
    sortDir
  );

  const globalMcps = applyNameSort(
    linkedGlobal.filter((e) => e.kind === "mcp" && match(e.name)),
    sortDir
  );
  const globalPlugins = applyNameSort(
    linkedGlobal.filter((e) => e.kind === "plugin" && match(e.name)),
    sortDir
  );

  return (
    <div className="space-y-2">
      {/* Skills：三态合并行 + 右侧来源徽章列 */}
      <div>
        <h4 className="text-muted-foreground mb-1 text-xs font-medium">
          Skills ({sortedSkillRows.length})
        </h4>
        <div className="space-y-1">
          {sortedSkillRows.length === 0 ? (
            <div className="text-muted-foreground px-2 py-1 text-[11px]">
              {t("resources.noSkillsHint")}
            </div>
          ) : (
            sortedSkillRows.map((row) => (
              <div
                key={row.name}
                className={`flex items-center justify-between gap-2 rounded px-2 py-1 text-xs ${
                  row.source === "native" ? "bg-muted" : "bg-accent/50"
                }`}
              >
                <span className="truncate">{formatSkillName(row.name)}</span>
                <span className="flex shrink-0 items-center gap-1">
                  {/* 原生技能保留导入动作（导入 SSOT 纳管） */}
                  {row.native && (
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-5 px-1 text-[10px]"
                      onClick={() => onImport(toolId, row.native!)}
                    >
                      <Import className="h-3 w-3" />
                      {t("common.import")}
                    </Button>
                  )}
                  <ResourceSourceBadge source={row.source} />
                </span>
              </div>
            ))
          )}
        </div>
      </div>

      {/* MCP：仅实际配置（已链接）项 + 全局仓库徽章 */}
      <div>
        <h4 className="text-muted-foreground mb-1 text-xs font-medium">
          MCP ({globalMcps.length})
        </h4>
        <div className="space-y-1">
          {globalMcps.length === 0 ? (
            <div className="text-muted-foreground px-2 py-1 text-[11px]">
              {t("resources.noMcpHint")}
            </div>
          ) : (
            globalMcps.map((m) => (
              <div
                key={m.id}
                className="bg-accent/50 flex items-center justify-between gap-2 rounded px-2 py-1 text-xs"
              >
                <span className="truncate">{m.name}</span>
                <ResourceSourceBadge source="repo" />
              </div>
            ))
          )}
        </div>
      </div>

      {/* Plugins：仅实际配置（已链接）项 + 全局仓库徽章 */}
      <div>
        <h4 className="text-muted-foreground mb-1 text-xs font-medium">
          Plugins ({globalPlugins.length})
        </h4>
        <div className="space-y-1">
          {globalPlugins.length === 0 ? (
            <div className="text-muted-foreground px-2 py-1 text-[11px]">
              {t("resources.noPluginHint")}
            </div>
          ) : (
            globalPlugins.map((p) => (
              <div
                key={p.id}
                className="bg-accent/50 flex items-center justify-between gap-2 rounded px-2 py-1 text-xs"
              >
                <span className="truncate">{p.name}</span>
                <ResourceSourceBadge source="repo" />
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
}

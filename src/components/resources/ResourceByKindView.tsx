// 「按资源」视图（资源管理页 UI 改版 A1 瘦身）：卡片只留仓库入口行（标题 + 搜索框 +
// 快捷跳转 + 从 Manifest 安装）与三段段头入口（Skill / MCP / 插件，计数留段头，点击
// 打开 ResourceKindDialog 并预选该类型）；原三段纵向折叠段与大表整体移入弹窗
// （ResourceKindDialog，逻辑原样随迁）。搜索框状态提升留卡片，过滤弹窗内列表；
// 弹窗内另渲染一枚同源搜索框（F3：模态遮罩盖住卡片这只，否则打开弹窗就只能关掉再改
// 关键词），两边共用同一份提升状态。
// 预设组区（PresetList）由父级 ExtensionList 在两视图下方共用渲染，不在本卡片内。
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { homeDir } from "@tauri-apps/api/path";
import { toast } from "sonner";
import { formatInvokeError } from "@/lib/invokeError";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FileJson, FolderOpen, ChevronRight } from "lucide-react";
import { useSsotResourcesQuery, SSOT_RESOURCES_KEY } from "@/lib/query/queries/resources";
import { ResourceKindDialog } from "./ResourceKindDialog";
import { KIND_META, matchesResourceSearch, type ResourceKind } from "./kindMeta";
import { ManifestInstallDialog } from "./ManifestInstallDialog";

/** 废止的折叠态 localStorage 键（A1：段头折叠机制随大表入弹窗而废止，一次性清理防遗留） */
const LEGACY_COLLAPSED_KEY = "mam.resourceView.collapsed";

export function ResourceByKindView() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const { data: resources } = useSsotResourcesQuery();
  const [search, setSearch] = useState("");
  // 资源大表弹窗（A2）：open + 点段头预选的类型
  const [dialogOpen, setDialogOpen] = useState(false);
  const [dialogKind, setDialogKind] = useState<ResourceKind>("skill");
  // 从 Manifest 安装路径弹窗（卡片仓库入口行的动作，随卡片留下）
  const [manifestDlgOpen, setManifestDlgOpen] = useState(false);
  const [manifestPath, setManifestPath] = useState("");
  const [installDlgPath, setInstallDlgPath] = useState<string | null>(null);
  const [installDlgOpen, setInstallDlgOpen] = useState(false);

  // 折叠态机制废止：一次性清掉旧 localStorage 键（老用户升级后不残留脏数据）
  useEffect(() => {
    try {
      localStorage.removeItem(LEGACY_COLLAPSED_KEY);
    } catch {
      // 存储不可用（隐私模式等）→ 跳过
    }
  }, []);

  /** 快捷跳转（用户反馈 wave33 Item B）：用系统文件管理器打开 兔维斯 仓库 /
   *  ~/.agents skill 目录。reveal_dir 后端白名单做词法前缀 + canonicalize 校验
   *  且不展开 ~，必须传绝对路径——homeDir() 取平台 home 现拼；目录不存在时
   *  后端 Err（message 含路径）→ formatInvokeError toast */
  const revealQuickJump = async (rel: ".tuvis/skills" | ".agents/skills") => {
    try {
      const home = await homeDir();
      // homeDir() 平台带尾斜杠不一（macOS 带 / Windows 不带），兼容两种拼接
      const path = home.endsWith("/") ? `${home}${rel}` : `${home}/${rel}`;
      await invoke("reveal_dir", { path });
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  if (!resources) {
    return <div className="text-muted-foreground py-4 text-xs">{t("common.loading")}</div>;
  }

  // 搜索判据与弹窗共用一个实现（kindMeta.matchesResourceSearch）：段头计数与弹窗 tab
  // 计数、行过滤永远同口径
  const matches = (r: { name: string; enabledTools: string[] }) => matchesResourceSearch(r, search);
  // 段头计数 = 搜索过滤后的段内条数（与弹窗内 tab 计数同口径）
  const counts: Record<ResourceKind, number> = {
    skill: resources.skills.filter(matches).length,
    mcp: resources.mcp.filter(matches).length,
    plugin: resources.plugins.filter(matches).length,
  };

  const openKind = (kind: ResourceKind) => {
    setDialogKind(kind);
    setDialogOpen(true);
  };

  return (
    <>
      <div className="bg-card rounded-lg border p-4">
        <h3 className="mb-3 text-sm font-semibold">{t("resources.repoTitle")}</h3>

        <div className="mb-3 flex items-center justify-between gap-2">
          <div className="flex items-center gap-1">
            {/* 搜索框留卡片（A1）：状态提升，过滤弹窗内三段列表 + 卡片段头计数；
                弹窗打开后由 DialogContent 里那枚同源框接管（F3，同一 value/onChange） */}
            <input
              type="text"
              placeholder={t("resources.searchPlaceholder")}
              value={search}
              onChange={(e) => setSearch(e.currentTarget.value)}
              className="h-7 w-40 rounded border px-2 text-xs"
            />
            {/* 快捷跳转（用户反馈 wave33 Item B）：兔维斯 仓库 / ~/.agents 目录直达 */}
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-1.5 text-[10px]"
              title={t("resources.openMamRepo")}
              aria-label={t("resources.openMamRepo")}
              onClick={() => void revealQuickJump(".tuvis/skills")}
            >
              <FolderOpen className="mr-1 h-3 w-3" />
              {t("resources.openMamRepo")}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-1.5 text-[10px]"
              title={t("resources.openAgentsDir")}
              aria-label={t("resources.openAgentsDir")}
              onClick={() => void revealQuickJump(".agents/skills")}
            >
              <FolderOpen className="mr-1 h-3 w-3" />
              {t("resources.openAgentsDir")}
            </Button>
          </div>
          <Button
            size="sm"
            variant="outline"
            className="h-6 px-2 text-[10px]"
            onClick={() => setManifestDlgOpen(true)}
          >
            <FileJson className="mr-1 h-3 w-3" />
            {t("resources.installFromManifest")}
          </Button>
        </div>

        {/* 三段段头入口（A1）：数字留段头，点段头开 Dialog 并预选该类型 */}
        <div>
          {KIND_META.map(({ kind, icon: Icon, countKey }) => (
            <button
              key={kind}
              type="button"
              onClick={() => openKind(kind)}
              className="hover:bg-accent/40 mb-2 flex w-full cursor-pointer items-center gap-2 rounded border px-3 py-2 text-left text-sm font-medium transition-colors last:mb-0"
            >
              <Icon className="h-4 w-4" />
              {t(countKey, { n: counts[kind] })}
              <ChevronRight className="text-muted-foreground ml-auto h-4 w-4" />
            </button>
          ))}
        </div>
      </div>

      {/* 资源大表弹窗（A2）：大表 + 类型切换 + 行级子弹窗整体迁入；搜索词受控传入
          （F3：弹窗内改词 → 列表与 tab 计数实时联动） */}
      <ResourceKindDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        initialKind={dialogKind}
        search={search}
        onSearchChange={setSearch}
      />

      {/* 从 Manifest 安装路径弹窗 */}
      <Dialog open={manifestDlgOpen} onOpenChange={setManifestDlgOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t("resources.installFromManifest")}</DialogTitle>
          </DialogHeader>
          <div className="py-2">
            <label className="text-xs font-medium">{t("resources.manifestPathLabel")}</label>
            <input
              value={manifestPath}
              onChange={(e) => setManifestPath(e.currentTarget.value)}
              placeholder={t("resources.manifestPathPlaceholder")}
              className="h-8 w-full rounded border px-2 text-xs"
            />
          </div>
          <DialogFooter className="gap-2">
            <Button variant="outline" size="sm" onClick={() => setManifestDlgOpen(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              size="sm"
              disabled={!manifestPath.trim()}
              onClick={() => {
                setInstallDlgPath(manifestPath.trim());
                setInstallDlgOpen(true);
                setManifestDlgOpen(false);
              }}
            >
              {t("common.confirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ManifestInstallDialog
        path={installDlgPath}
        open={installDlgOpen}
        onOpenChange={setInstallDlgOpen}
        onInstalled={async () => {
          // Manifest 安装成功 → 失效 SSOT 资源查询（与原 refresh() 同语义，卡片计数/弹窗表格随之刷新）
          try {
            await qc.invalidateQueries({ queryKey: SSOT_RESOURCES_KEY });
          } catch (e) {
            toast.error(t("common.operationFailed", { error: formatInvokeError(e, t) }));
          }
        }}
      />
    </>
  );
}

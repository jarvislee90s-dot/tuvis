// 预设只读预览弹窗（资源管理页 UI 改版 B5）：预设卡名称行 👁 入口，快速看清预设装了什么。
// 按 Skill / MCP / 插件 分组列名；每项佩戴来源徽章（原生 / 全局仓库）——来源查
// presetExtensions 名册（isNative 判原生；查无的残留项不佩徽章，如实呈现）。
// 纯只读：无任何写动作，关闭即散。
import { useTranslation } from "react-i18next";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ResourceSourceBadge } from "@/components/resources/ResourceSourceBadge";
import type { PresetRecord } from "@/types/preset";
import type { ExtensionWithAssignments } from "@/types/extension";

/** 分组顺序与组头文案（组头复用既有 resources.*Count 计数文案，不新增 key） */
const KIND_GROUPS: { kind: string; countKey: string }[] = [
  { kind: "skill", countKey: "resources.skillsCount" },
  { kind: "mcp", countKey: "resources.mcpsCount" },
  { kind: "plugin", countKey: "resources.pluginsCount" },
];

export function PresetPreviewDialog({
  preset,
  extensions,
  open,
  onOpenChange,
}: {
  preset: PresetRecord | null;
  /** 套件名册（经 ExtensionList → PresetList 透传），供来源徽章判定 */
  extensions: ExtensionWithAssignments[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation();
  const groups = KIND_GROUPS.map(({ kind, countKey }) => ({
    kind,
    countKey,
    items: (preset?.items ?? []).filter((i) => i.kind === kind),
  })).filter((g) => g.items.length > 0);

  return (
    <Dialog open={open && preset !== null} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby={undefined} className="max-w-md">
        <DialogHeader>
          <DialogTitle className="truncate pr-6 text-sm">{preset?.name}</DialogTitle>
          {preset?.description ? (
            <DialogDescription className="truncate text-xs">{preset.description}</DialogDescription>
          ) : null}
        </DialogHeader>
        <div className="space-y-2">
          {groups.map((group) => (
            <div key={group.kind}>
              <h4 className="text-muted-foreground mb-1 text-xs font-medium">
                {t(group.countKey, { n: group.items.length })}
              </h4>
              <div className="space-y-1">
                {group.items.map((item) => {
                  // 来源徽章：查名册判原生/全局仓库；查无（列表未载入或残留项）→ 不佩徽章
                  const ext = extensions.find((e) => e.id === item.extensionId);
                  return (
                    <div
                      key={`${item.extensionId}::${item.kind}`}
                      className="bg-accent/50 flex items-center justify-between gap-2 rounded px-2 py-1 text-xs"
                    >
                      <span className="truncate">{item.extensionName}</span>
                      {ext && <ResourceSourceBadge source={ext.isNative ? "native" : "repo"} />}
                    </div>
                  );
                })}
              </div>
            </div>
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}

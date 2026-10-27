import { useState, useEffect, useCallback } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "@/components/ui/button";
import { WorkspacePageHeader } from "@/components/common/WorkspacePageHeader";
import { Scan, LayoutGrid, List } from "lucide-react";
import type { ExtensionWithAssignments } from "@/types/extension";
import { ResourceByKindView } from "./ResourceByKindView";
import { ResourceByToolView } from "./ResourceByToolView";
import { ImportDialog } from "./ImportDialog";
import { HealthCheckCard } from "./HealthCheckCard";
import { PresetList } from "../presets/PresetList";

export function ExtensionList() {
  const { t } = useTranslation();
  const [view, setView] = useState<"byKind" | "byTool">("byKind");
  const [extensions, setExtensions] = useState<ExtensionWithAssignments[]>([]);
  const [showImport, setShowImport] = useState(false);

  const load = useCallback(async () => {
    try {
      const data = await invoke<ExtensionWithAssignments[]>("list_extensions_with_assignments");
      setExtensions(data);
    } catch (e) {
      console.error("Failed to load extensions:", e);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  return (
    <div className="space-y-4">
      {/* 统一页头（Task 8）：双视图切换 + 扫描原生资源收进 actions 槽 */}
      <WorkspacePageHeader
        title={t("workspace.page.resources")}
        actions={
          <>
            <div className="flex gap-1">
              <Button
                size="sm"
                variant={view === "byKind" ? "default" : "outline"}
                className="h-7 px-2 text-xs"
                onClick={() => setView("byKind")}
              >
                <List className="mr-1 h-3 w-3" />
                {t("resources.byKind")}
              </Button>
              <Button
                size="sm"
                variant={view === "byTool" ? "default" : "outline"}
                className="h-7 px-2 text-xs"
                onClick={() => setView("byTool")}
              >
                <LayoutGrid className="mr-1 h-3 w-3" />
                {t("resources.byTool")}
              </Button>
            </div>
            <Button
              size="sm"
              variant="outline"
              className="h-7 px-2 text-xs"
              onClick={() => setShowImport(true)}
            >
              <Scan className="mr-1 h-3 w-3" />
              {t("resources.scanNative")}
            </Button>
          </>
        }
      />

      {/* 一致性体检卡片（spec §13）：存在未决差异时卡片标题处自带角标 */}
      <HealthCheckCard />

      {/* View content */}
      {view === "byKind" ? <ResourceByKindView /> : <ResourceByToolView />}

      {/* Presets */}
      <PresetList extensions={extensions} />

      {/* Import dialog */}
      <ImportDialog open={showImport} onClose={() => setShowImport(false)} onImported={load} />
    </div>
  );
}

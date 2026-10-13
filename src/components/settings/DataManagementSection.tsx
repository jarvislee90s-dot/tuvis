// 数据管理分区（2026-09-20 用户要求）：桌面端 兔维斯 存储数据的统一管理入口。
// 首版只管移动端附件（各项目 .tuvis-attachments/<会话>/）：列出占用 + 按项目清理；
// 审计/HANDOFF 等其余数据进治理台账（spec 附录），实现逐期跟进。
import { SETTINGS_CARD_TITLE } from "@/components/settings/typography";
import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { Button } from "@/components/ui/button";

interface AttachmentProjectStats {
  project: string;
  files: number;
  bytes: number;
}

/** 单项目占用提醒阈值：超过在卡片标提示（不阻断——清理权在用户） */
const PROJECT_SIZE_WARN_BYTES = 200 * 1024 * 1024;

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function DataManagementSection() {
  const { t } = useAppTranslation();
  const [projects, setProjects] = useState<AttachmentProjectStats[]>([]);
  const [loadError, setLoadError] = useState(false);
  const [loading, setLoading] = useState(false);
  /** 二次确认态：记录当前待清理的项目路径（同一时刻至多一个在确认中） */
  const [confirming, setConfirming] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const rows = await invoke<AttachmentProjectStats[]>("list_attachment_projects");
      setProjects(rows ?? []);
      setLoadError(false);
    } catch (e) {
      console.error("list_attachment_projects failed:", e);
      setLoadError(true);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const clean = useCallback(async (project: string) => {
    try {
      await invoke("clean_attachment_project", { project });
      setProjects((prev) => prev.filter((p) => p.project !== project));
    } catch (e) {
      console.error("clean_attachment_project failed:", e);
    } finally {
      setConfirming(null);
    }
  }, []);

  return (
    <div className="overflow-hidden rounded-xl border">
      {/* 卡头条（2026-10-11 两级标题拉开）：h2 保留档位常量搬进条内；说明收条右侧 */}
      <div className="bg-muted/40 flex items-center justify-between gap-2 border-b px-4 py-2.5">
        <h2 className={SETTINGS_CARD_TITLE}>{t("settings.dataManagement.title")}</h2>
        <div className="flex min-w-0 items-baseline gap-2">
          <span className="text-muted-foreground truncate text-xs">
            {t("settings.dataManagement.hint")}
          </span>
          <Button variant="outline" size="sm" onClick={() => void refresh()} disabled={loading}>
            {t("settings.dataManagement.refresh")}
          </Button>
        </div>
      </div>

      <div className="p-3">
        {loadError ? (
          <p className="text-sm text-red-500" data-testid="data-mgmt-load-error">
            {t("settings.dataManagement.loadError")}
          </p>
        ) : projects.length === 0 ? (
          <p className="text-sm text-slate-500 dark:text-slate-400" data-testid="data-mgmt-empty">
            {t("settings.dataManagement.empty")}
          </p>
        ) : (
          <ul className="space-y-2" data-testid="data-mgmt-list">
            {projects.map((p) => (
              <li
                key={p.project}
                className="flex items-center gap-2 rounded-lg border border-slate-200 px-3 py-2 text-sm dark:border-slate-800"
              >
                <span className="min-w-0 flex-1 truncate" title={p.project}>
                  {p.project}
                </span>
                <span className="shrink-0 text-xs text-slate-500 dark:text-slate-400">
                  {p.files} · {formatBytes(p.bytes)}
                </span>
                {p.bytes > PROJECT_SIZE_WARN_BYTES && (
                  <span className="shrink-0 text-xs text-amber-600 dark:text-amber-400">
                    {t("settings.dataManagement.overThreshold")}
                  </span>
                )}
                {confirming === p.project ? (
                  <>
                    <Button variant="destructive" size="sm" onClick={() => void clean(p.project)}>
                      {t("settings.dataManagement.clean")}
                    </Button>
                    <Button variant="ghost" size="sm" onClick={() => setConfirming(null)}>
                      ×
                    </Button>
                  </>
                ) : (
                  <Button variant="outline" size="sm" onClick={() => setConfirming(p.project)}>
                    {t("settings.dataManagement.clean")}
                  </Button>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

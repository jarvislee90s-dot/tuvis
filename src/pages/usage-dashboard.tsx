// 旧 `/usage` 独立窗口入口的兼容跳转（2026-10-26 计划 Task 7；spec §2.4）。
//
// 四页同窗后**不再创建** usage-dashboard 窗口；但旧入口（书签式直开 `/usage`、
// 旧版本残留窗口、手输 URL）仍可能落到本路由——行为：请求主窗口切到「模型用量」
// 页签（同一 revision 排序队列），然后关闭自身窗口（主窗之外才关）。**绝不**在此
// 嵌套用量外壳（WindowFrame/标题栏/分区一律不渲染），真实界面在主窗口的用量页签。
import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { openWorkspaceTab } from "@/lib/workspaceNavigation";

export default function UsageDashboardPage() {
  const { t } = useTranslation();

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      // 先确保主窗口落到用量页签（失败也继续关窗：目标已入队，主窗 peek 会恢复）
      try {
        await openWorkspaceTab("usage");
      } catch (error) {
        console.warn("[usage] 旧 /usage 入口导航主窗失败：", error);
      }
      if (cancelled) return;
      // 主窗之外才关自己（直开主窗 URL 的场景不动窗口）。close 会被托盘的
      // CloseRequested 拦截成 hide，故用 destroy 彻底销毁这个历史残留窗。
      try {
        const current = getCurrentWebviewWindow();
        if (current.label !== "main") await current.destroy();
      } catch (error) {
        console.warn("[usage] 旧 /usage 入口关闭自身失败：", error);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="text-muted-foreground flex h-screen items-center justify-center p-6 text-sm">
      <p>{t("usage.redirectNotice")}</p>
    </div>
  );
}

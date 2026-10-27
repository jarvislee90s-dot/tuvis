// 桌面统一页头（2026-10-26 计划 Task 8，spec §3）：四页（消息看板/工具资源/
// 模型用量/输出速度）共用的唯一页头形态——标题（h1，18px）+ 可选次级说明
// （12–13px 次文字）+ 可选右侧动作槽。由各页把原页面顶部的摘要/操作收进
// actions，替代各自为政的页首行；不嵌套 WindowFrame、不重复标题栏。
import type { ReactNode } from "react";

type WorkspacePageHeaderProps = {
  title: string;
  description?: string;
  actions?: ReactNode;
};

export function WorkspacePageHeader({ title, description, actions }: WorkspacePageHeaderProps) {
  return (
    <header className="flex items-start justify-between gap-3">
      {/* 标题块 shrink-0 + h1 nowrap（实机反馈轮 2026-10-27 #3b）：页标题是固定短词
          （四页各 ≤4 字），永不参与收缩——此前动作槽 shrink-0、标题可收缩，
          用量页导出工具栏超宽时标题被挤到 1 字宽、CJK 逐字换行成竖排「模 型 用 量」 */}
      <div className="shrink-0">
        <h1 className="text-lg leading-tight font-semibold whitespace-nowrap">{title}</h1>
        {description ? (
          <p className="text-muted-foreground mt-0.5 text-xs leading-snug">{description}</p>
        ) : null}
      </div>
      {/* 动作槽 min-w-0 + flex-wrap（同上）：超宽动作换到下一行，而不是挤标题 */}
      {actions ? (
        <div className="flex min-w-0 flex-wrap items-center justify-end gap-2">{actions}</div>
      ) : null}
    </header>
  );
}

export default WorkspacePageHeader;

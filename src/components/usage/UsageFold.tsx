// 可折叠卡壳（Task 12）——C 分布 / D 费用 / E 工作活动共用。
//
// 关键语义：
//  * 卡壳**恒在**：折叠时保留标题、摘要与展开入口（空态同样保留卡头）；
//  * 折叠按钮是原生 `Button` + `aria-expanded` / `aria-controls`，`aria-label` 用标题
//    （Chevron 图标不是唯一可访问名称）；
//  * `content id` 与 `aria-controls` 一致；折叠时正文**不挂载**（普通截图不会出现折叠内容）。
import type { ReactNode } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";

export interface UsageFoldProps {
  /** 正文 id（同时用于测试与 `aria-controls`） */
  id: string;
  title: string;
  open: boolean;
  onOpenChange(open: boolean): void;
  /** 卡头摘要（如工作活动的会话数/工具调用次数）；折叠时仍可见 */
  summary?: ReactNode;
  /** 卡头右侧、折叠按钮之前的操作区（如分布维度切换） */
  actions?: ReactNode;
  children: ReactNode;
}

export function UsageFold({
  id,
  title,
  open,
  onOpenChange,
  summary,
  actions,
  children,
}: UsageFoldProps) {
  const contentId = `usage-fold-content-${id}`;
  return (
    <Card data-testid={`usage-fold-${id}`} className="gap-3 py-4">
      <CardHeader className="gap-1 px-4">
        <CardTitle>{title}</CardTitle>
        {summary ? (
          <div data-testid={`usage-fold-summary-${id}`} className="text-muted-foreground text-xs">
            {summary}
          </div>
        ) : null}
        <CardAction>
          <div className="flex items-center gap-1.5">
            {actions}
            <Button
              variant="ghost"
              size="sm"
              aria-expanded={open}
              aria-controls={contentId}
              aria-label={title}
              data-testid={`usage-fold-toggle-${id}`}
              onClick={() => onOpenChange(!open)}
            >
              {open ? (
                <ChevronDown className="h-4 w-4" aria-hidden="true" />
              ) : (
                <ChevronRight className="h-4 w-4" aria-hidden="true" />
              )}
            </Button>
          </div>
        </CardAction>
      </CardHeader>
      {open ? (
        <CardContent id={contentId} data-testid={contentId} className="px-4">
          {children}
        </CardContent>
      ) : null}
    </Card>
  );
}

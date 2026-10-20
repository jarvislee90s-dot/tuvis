// 高级配置的 Info 帮助图标：hover/focus 显示说明，不挤压布局。
//
// 纪律：触发器是**独立 button**（不与折叠头 button 嵌套），有 aria-label；
// 说明放在 Radix Tooltip 内容里，由 Provider 统一挂载（见 main.tsx）。
import type { ReactNode } from "react";
import { Info } from "lucide-react";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

export interface UsageInfoTipProps {
  /** 触发按钮的可访问名称（不是说明正文） */
  label: string;
  children: ReactNode;
  testId?: string;
  className?: string;
}

export function UsageInfoTip({ label, children, testId, className }: UsageInfoTipProps) {
  return (
    // 组件自带 Provider：设置页/费用卡在测试或独立渲染时不必再包全局 Provider；
    // 应用级 Provider（main.tsx）仍负责跨组件的 skip-delay 行为。
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>
          <button
            type="button"
            aria-label={label}
            data-testid={testId}
            className={cn(
              "text-muted-foreground hover:text-foreground focus-visible:ring-ring inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-full align-middle focus-visible:ring-2 focus-visible:outline-none",
              className
            )}
            onClick={(event) => event.stopPropagation()}
          >
            <Info className="h-3.5 w-3.5" aria-hidden="true" />
          </button>
        </TooltipTrigger>
        <TooltipContent side="top" align="start">
          <div className="space-y-1">{children}</div>
        </TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}

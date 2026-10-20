// 轻量受控折叠区（不是 Card）：用于「高级覆盖」子区，避免卡中卡。
//
// 标题按钮与 chevron 按钮是两个独立 button（Info 图标也是独立 button），
// 不做 button 嵌套；两者都带 aria-expanded / aria-controls。
import type { ReactNode } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export interface UsageDisclosureProps {
  id: string;
  title: string;
  summary?: ReactNode;
  info?: ReactNode;
  open: boolean;
  onOpenChange(open: boolean): void;
  children: ReactNode;
  testId: string;
  className?: string;
  bodyClassName?: string;
}

export function UsageDisclosure({
  id,
  title,
  summary,
  info,
  open,
  onOpenChange,
  children,
  testId,
  className,
  bodyClassName,
}: UsageDisclosureProps) {
  const contentId = `${id}-content`;
  return (
    <section data-testid={testId} className={cn("rounded-md border", className)}>
      <div className="flex items-start gap-1.5 px-2 py-1.5">
        <button
          type="button"
          data-testid={`${testId}-toggle`}
          className="min-w-0 flex-1 text-left"
          aria-expanded={open}
          aria-controls={open ? contentId : undefined}
          onClick={() => onOpenChange(!open)}
        >
          <span className="text-sm font-medium">{title}</span>
          {summary ? (
            <span className="text-muted-foreground mt-0.5 block text-xs leading-relaxed">
              {summary}
            </span>
          ) : null}
        </button>
        {info}
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-6 w-6 shrink-0 p-0"
          data-testid={`${testId}-chevron`}
          aria-expanded={open}
          aria-controls={open ? contentId : undefined}
          aria-label={title}
          onClick={() => onOpenChange(!open)}
        >
          {open ? (
            <ChevronDown className="h-4 w-4" aria-hidden="true" />
          ) : (
            <ChevronRight className="h-4 w-4" aria-hidden="true" />
          )}
        </Button>
      </div>
      {open ? (
        <div
          id={contentId}
          data-testid={`${testId}-content`}
          className={cn("border-t px-2 py-2", bodyClassName)}
        >
          {children}
        </div>
      ) : null}
    </section>
  );
}

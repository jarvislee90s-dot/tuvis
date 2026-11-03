// 工具特殊配置折叠卡（设置页 ③）：与 ZCode 卡同款卡头/展开语义。
// 默认折叠；卡头本身是唯一展开入口（原生 button，Enter/Space 可用）。
import { useState, type ReactNode } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { ToolIcon } from "@/components/common/ToolIcon";
import { SETTINGS_FIELD } from "@/components/settings/typography";

interface ToolConfigCardProps {
  toolId: string;
  title: string;
  children: ReactNode;
}

export function ToolConfigCard({ toolId, title, children }: ToolConfigCardProps) {
  const [expanded, setExpanded] = useState(false);
  return (
    <div data-tool-card={toolId} className="mt-2 overflow-hidden rounded-xl border">
      <button
        type="button"
        data-testid={`tool-config-header-${toolId}`}
        aria-expanded={expanded}
        aria-controls={`tool-config-panel-${toolId}`}
        onClick={() => setExpanded((value) => !value)}
        className="bg-muted/40 hover:bg-muted flex w-full items-center gap-2 px-4 py-2.5 text-left"
      >
        <ToolIcon toolId={toolId} size={18} />
        <span className={SETTINGS_FIELD}>{title}</span>
        {expanded ? (
          <ChevronDown className="text-muted-foreground ml-auto flex-none" size={16} aria-hidden />
        ) : (
          <ChevronRight className="text-muted-foreground ml-auto flex-none" size={16} aria-hidden />
        )}
      </button>
      {expanded && (
        <div
          id={`tool-config-panel-${toolId}`}
          data-testid={`tool-config-panel-${toolId}`}
          role="region"
          aria-label={title}
          className="border-t p-4"
        >
          {children}
        </div>
      )}
    </div>
  );
}

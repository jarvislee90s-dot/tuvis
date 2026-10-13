// 桌面看板「按工具筛选」chips（2026-10-11）
//
// 形态（用户截图定案）：「全部」+ 固定 8 工具，单选、溢出换行；切到扩展 tab 由
// 父级条件渲染整排隐藏。图标/文案与 SessionCard 徽章**同源**（AGENT_BADGE 一张
// 表出 label + 图标 + chip 图标色），选中态 = 近黑主控 pills（对齐移动端
// board-logic「选中 = 近黑主控」的中性化口径，品牌识别色只留在图标上）。
import { useAppTranslation } from "@/hooks/use-app-translation";
import { AGENT_BADGE } from "@/lib/agentBadge";
import { SUPPORTED_TOOLS } from "@/config/constants";
import type { ToolFilter } from "@/lib/toolFilter";
import { cn } from "@/lib/utils";

const CHIP_SELECTED = "border-primary bg-primary text-primary-foreground";
const CHIP_UNSELECTED = "hover:bg-accent/50 border-border bg-card text-foreground";
const CHIP_BASE = `flex items-center gap-1.5 rounded-full border px-3 py-1 text-xs transition-colors`;

export function ToolFilterChips({
  value,
  onChange,
}: {
  value: ToolFilter;
  onChange: (filter: ToolFilter) => void;
}) {
  const { t } = useAppTranslation();
  return (
    <div className="flex flex-wrap items-center gap-2" data-testid="tool-chips">
      <button
        type="button"
        data-testid="tool-chip-all"
        aria-pressed={value === "all"}
        onClick={() => onChange("all")}
        className={cn(CHIP_BASE, value === "all" ? CHIP_SELECTED : CHIP_UNSELECTED)}
      >
        {t("home.filterAll")}
      </button>
      {SUPPORTED_TOOLS.map((tool) => {
        // SUPPORTED_TOOLS 与 AGENT_BADGE 键集有穷尽测试对齐（tests/agentBadge.test.tsx）；
        // 防御性 null：键集漂移时不崩，该工具 chip 缺席可见
        const badge = AGENT_BADGE[tool];
        if (!badge) return null;
        const selected = value === tool;
        const Icon = badge.Icon;
        return (
          <button
            key={tool}
            type="button"
            data-testid={`tool-chip-${tool}`}
            aria-pressed={selected}
            onClick={() => onChange(tool)}
            className={cn(CHIP_BASE, selected ? CHIP_SELECTED : CHIP_UNSELECTED)}
          >
            <Icon className={`h-3.5 w-3.5 ${badge.iconClass}`} />
            {badge.label}
          </button>
        );
      })}
    </div>
  );
}

// 桌面看板「按工具筛选」chips（2026-10-11）
//
// 形态（用户截图定案）：「全部」+ 7 工具（openclaw 不支持消息提醒，2026-10-14
// 起不入筛选），单选、溢出换行；切到扩展 tab 由父级条件渲染整排隐藏。图标/文案
// 与 SessionCard 徽章**同源**（AGENT_BADGE 一张表出 label + 图标 + chip 图标色），
// 选中态 = 近黑主控 pills（对齐移动端 board-logic「选中 = 近黑主控」的中性化口径，
// 品牌识别色只留在图标上）。
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
        // SUPPORTED_TOOLS ⊆ AGENT_BADGE 键集有单向包含测试（tests/agentBadge.test.tsx；
        // openclaw 因退出筛选正靠该不对称留存在 BADGE 表里，另有显式存续锁）；
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

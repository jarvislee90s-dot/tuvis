// 工具图标 — AGENT_BADGE 唯一设计语言的薄委托壳（2026-10-14 统一批）：
// 图标 / 配色 / 名字全部来自 agentBadge 表，本组件只负责 toolId 查表 + size 盒 +
// unknown 回退 claude。资源 / 预设 / 设置 / 移动卡片等调用点 props API 不变。
// 原自绘 SVG + --tool-* CSS 变量体系随统一批退役（变量表已删）。
import { AGENT_BADGE } from "@/lib/agentBadge";

interface ToolIconProps {
  toolId: string;
  className?: string;
  size?: number;
}

export function ToolIcon({ toolId, className = "", size = 16 }: ToolIconProps) {
  const badge = AGENT_BADGE[toolId] ?? AGENT_BADGE.claude;
  const Icon = badge.Icon;
  return (
    <span
      className={`inline-flex items-center justify-center ${className}`}
      style={{ width: size, height: size }}
    >
      <Icon className={`h-full w-full ${badge.iconClass}`} />
    </span>
  );
}

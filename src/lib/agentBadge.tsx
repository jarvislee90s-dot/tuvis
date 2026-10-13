// 工具徽标映射 — SessionCard 与通知浮窗共用（配色：codex 紫 / claude 橙 / opencode 灰白 / openclaw 灰 / kimi 天蓝 / workbuddy 品牌绿 / zcode 品牌蓝紫）
import type { ComponentType } from "react";
import { ClaudeIcon, KimiIcon, OpenAIIcon, OpenCodeIcon } from "@/components/icons/BrandIcons";
import { ToolIcon } from "@/components/common/ToolIcon";

export interface AgentBadge {
  label: string;
  className: string;
  /** chips 图标专用文字色（2026-10-11 看板筛选）：中性 chip 底上的品牌识别色，
   *  亮/暗两态各自达标——与 className 的徽章文字色**同表分列**而非复用同值：
   *  徽章自带色底（如 opencode 深灰底），其 text 类直接落在白 chip 上会不可见。
   *  currentColor 图标消费；固定色图标（ToolIcon 适配器三类）忽略 className */
  iconClass: string;
  Icon: ComponentType<{ className?: string }>;
}

/** ToolIcon(toolId) → AgentBadge.Icon（className 形参）适配：WorkBuddy 专属品牌图标 */
function ToolIconworkbuddyAdapter({ className: _className }: { className?: string }) {
  return <ToolIcon toolId="workbuddy" size={14} />;
}

/** ToolIcon(toolId) → AgentBadge.Icon（className 形参）适配：ZCode 专属品牌图标 */
function ToolIconZCodeAdapter({ className: _className }: { className?: string }) {
  return <ToolIcon toolId="zcode" size={14} />;
}

/** ToolIcon(toolId) → AgentBadge.Icon（className 形参）适配：DSH 专属品牌图标 */
function ToolIconDshAdapter({ className: _className }: { className?: string }) {
  return <ToolIcon toolId="dsh" size={14} />;
}

/** 会话显示名的唯一命名源：主界面徽标、通知浮窗、历史面板统一引用。
 *  codex 区分桌面版（Codex APP）与命令行（Codex CLI），其余工具单一名称。 */
export function getAgentLabel(agentType: string, form?: string): string {
  if (agentType === "codex") return form === "app" ? "Codex APP" : "Codex CLI";
  if (agentType === "claude") return "Claude";
  if (agentType === "opencode") return "OpenCode";
  if (agentType === "openclaw") return "OpenClaw";
  if (agentType === "kimi") return "Kimi Code";
  if (agentType === "workbuddy") return "WorkBuddy";
  if (agentType === "zcode") return "ZCode";
  if (agentType === "dsh") return "DSH";
  return agentType;
}

export const AGENT_BADGE: Record<string, AgentBadge> = {
  claude: {
    label: "Claude",
    className: "border-orange-500/30 bg-orange-500/15 text-orange-400",
    iconClass: "text-orange-500 dark:text-orange-400",
    Icon: ClaudeIcon,
  },
  codex: {
    label: "Codex",
    className: "border-purple-500/30 bg-purple-500/15 text-purple-400",
    iconClass: "text-purple-500 dark:text-purple-400",
    Icon: OpenAIIcon,
  },
  opencode: {
    label: "OpenCode",
    className: "border-zinc-500/40 bg-zinc-800/80 text-zinc-100",
    iconClass: "text-zinc-500 dark:text-zinc-300",
    Icon: OpenCodeIcon,
  },
  openclaw: {
    label: "OpenClaw",
    className: "border-gray-500/30 bg-gray-500/15 text-gray-300",
    iconClass: "text-gray-500 dark:text-gray-300",
    Icon: OpenCodeIcon, // 无品牌素材，暂用占位图标，后续替换
  },
  kimi: {
    label: "Kimi Code",
    className: "border-sky-500/30 bg-sky-500/15 text-sky-400",
    iconClass: "text-sky-500 dark:text-sky-300",
    Icon: KimiIcon,
  },
  workbuddy: {
    label: "WorkBuddy",
    // 品牌绿（P2-10：官方图标几何重绘，与 ToolIcon 同源）
    className: "border-emerald-500/30 bg-emerald-500/15 text-emerald-400",
    iconClass: "text-emerald-500 dark:text-emerald-400",
    Icon: ToolIconworkbuddyAdapter,
  },
  zcode: {
    label: "ZCode",
    // 品牌蓝紫（官方图标几何重绘，与 ToolIcon 同源渐变 #3B5BFD→#8A4FF5）
    className: "border-indigo-500/30 bg-indigo-500/15 text-indigo-400",
    iconClass: "text-indigo-500 dark:text-indigo-400", // 适配器固定色，此列备 icon 换 currentColor 形态
    Icon: ToolIconZCodeAdapter,
  },
  dsh: {
    label: "DSH",
    // 品牌深蓝（DeepSeek #4D6BFE，与 ToolIcon 同源；与 kimi 天蓝/zcode 蓝紫拉开）
    className: "border-blue-500/30 bg-blue-500/15 text-blue-400",
    iconClass: "text-blue-500 dark:text-blue-400",
    Icon: ToolIconDshAdapter,
  },
};

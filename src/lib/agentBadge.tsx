// 工具徽标映射 — 全端唯一设计语言表（2026-10-14 统一批）：桌面 chips / 会话卡 /
// 通知直接消费本表；资源 / 预设 / 设置 / 移动卡片经 ToolIcon 薄委托消费。图标
// canonical 出处 = BrandIcons.tsx，本表定 label / 徽章 tint / 图标色 / 品牌裸色。
import type { ComponentType } from "react";
import {
  ClaudeIcon,
  DshIcon,
  KimiIcon,
  OpenAIIcon,
  OpenClawIcon,
  OpenCodeIcon,
  WorkBuddyIcon,
  ZCodeIcon,
} from "@/components/icons/BrandIcons";

export interface AgentBadge {
  label: string;
  className: string;
  /** chips 等中性底的图标文字色（2026-10-11 看板筛选）：currentColor 图标消费；
   *  固定色图标（opencode/zcode/dsh/workbuddy/openclaw）忽略此列 */
  iconClass: string;
  /** 品牌识别色裸值（浅色档）：远程外观预览檐色等需要原始色值的场景 */
  brandHex: string;
  Icon: ComponentType<{ className?: string }>;
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
    brandHex: "#F97316",
    Icon: ClaudeIcon,
  },
  codex: {
    label: "Codex",
    className: "border-purple-500/30 bg-purple-500/15 text-purple-400",
    iconClass: "text-purple-500 dark:text-purple-400",
    brandHex: "#A855F7",
    Icon: OpenAIIcon,
  },
  opencode: {
    label: "OpenCode",
    className: "border-zinc-500/40 bg-zinc-800/80 text-zinc-100",
    iconClass: "text-zinc-500 dark:text-zinc-300",
    brandHex: "#71717A",
    Icon: OpenCodeIcon,
  },
  openclaw: {
    label: "OpenClaw",
    // 猩红（2026-10-14 统一批：机器人图标随 ToolIcon 实物搬迁为红底，
    // 徽章 tint 由灰 harmonize 到同源红 #D2453C——台账/移动卡檐同色）
    className: "border-red-500/30 bg-red-500/15 text-red-400",
    iconClass: "text-red-500 dark:text-red-400",
    brandHex: "#D2453C",
    Icon: OpenClawIcon,
  },
  kimi: {
    label: "Kimi Code",
    className: "border-sky-500/30 bg-sky-500/15 text-sky-400",
    iconClass: "text-sky-500 dark:text-sky-300",
    brandHex: "#0EA5E9",
    Icon: KimiIcon,
  },
  workbuddy: {
    label: "WorkBuddy",
    // 品牌绿（官方图标参考图重绘 2026-10-14）
    className: "border-emerald-500/30 bg-emerald-500/15 text-emerald-400",
    iconClass: "text-emerald-500 dark:text-emerald-400",
    brandHex: "#10B981",
    Icon: WorkBuddyIcon,
  },
  zcode: {
    label: "ZCode",
    // 品牌蓝紫（图标为官方炭灰方白 Z，tint 保留蓝紫识别色）
    className: "border-indigo-500/30 bg-indigo-500/15 text-indigo-400",
    iconClass: "text-indigo-500 dark:text-indigo-400",
    brandHex: "#3B5BFD",
    Icon: ZCodeIcon,
  },
  dsh: {
    label: "DSH",
    // 品牌深蓝（DeepSeek #4D6BFE，与鲸标/移动卡檐同源；与 kimi 天蓝/zcode 蓝紫拉开）
    className: "border-blue-500/30 bg-blue-500/15 text-blue-400",
    iconClass: "text-blue-500 dark:text-blue-400",
    brandHex: "#4D6BFE",
    Icon: DshIcon,
  },
};

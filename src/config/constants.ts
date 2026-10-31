export const EVENT_FRESHNESS_THRESHOLD = 30;
export const SESSION_STATUS = {
  RUNNING: "running",
  WAITING: "waiting",
  IDLE: "idle",
  COMPLETED: "completed",
  UNKNOWN: "unknown",
} as const;
export const EXTENSION_KIND = { SKILL: "skill", MCP: "mcp", PLUGIN: "plugin" } as const;
// 债修复（ZCode 接入轮）：原清单缺 workbuddy（agentBadge 遍历测试的覆盖面漏了
// 第六工具）。本清单 = **看板筛选 chips** 的工具序；后端 TOOL_IDS 另含 openclaw
// （不支持消息提醒，2026-10-14 用户裁决不入看板筛选；其会话卡/通知徽标仍由
// AGENT_BADGE 提供）。新工具接入须同步此清单与后端 TOOL_IDS
export const SUPPORTED_TOOLS = [
  "claude",
  "codex",
  "opencode",
  "kimi",
  "workbuddy",
  "zcode",
  "dsh",
] as const;

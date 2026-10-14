// 桌面看板「按工具筛选」（2026-10-11）纯函数层：过滤 + localStorage 记忆。
// 零 React / 零 Tauri 依赖（对齐移动端 board-logic.ts 的纯函数纪律）；过滤逻辑
// 与移动端 filterByAgent 同口径，chips 顺序/文案由组件侧复用 agentBadge 同源给出。
import { SUPPORTED_TOOLS } from "@/config/constants";
import type { AgentType, Session } from "@/types/session";

export type ToolFilter = AgentType | "all";

const STORAGE_KEY = "board.toolFilter";

/** 过滤会话："all" 原样返回（引用保持，不过滤）；其余按 agentType 严格相等 */
export function filterByAgent(sessions: Session[], filter: ToolFilter): Session[] {
  if (filter === "all") return sessions;
  return sessions.filter((s) => s.agentType === filter);
}

/** 持久化读：空/损坏/超出工具清单的值一律回落 "all"——防御 localStorage
 *  脏数据，AgentType 增删值后旧残留也不会把筛选卡死在不存在的工具上
 *  （含 2026-10-14 起 openclaw 移出筛选清单后存量 `"openclaw"` 的自动回落） */
export function loadToolFilter(): ToolFilter {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw === "all") return "all";
    if (raw && (SUPPORTED_TOOLS as readonly string[]).includes(raw)) {
      return raw as ToolFilter;
    }
  } catch {
    /* localStorage 不可用（隐私模式等）：回落默认 */
  }
  return "all";
}

/** 持久化写：失败静默（记忆是体验增强，不阻塞筛选交互本身） */
export function saveToolFilter(filter: ToolFilter): void {
  try {
    localStorage.setItem(STORAGE_KEY, filter);
  } catch {
    /* 忽略 */
  }
}

// tests/toolLabelParity.test.ts — 工具显示名双源互锁（2026-10-14 徽标统一批新增）
// 背景：工具显示名有两处**独立定义**（桌面组件与移动 bundle 隔离，移动不从桌面
// import，属既定架构纪律）——桌面 `src/lib/agentBadge.tsx` 的 AGENT_BADGE[k].label
// 与移动 `src/mobile/board-logic.ts` 的 TOOL_LABELS[k]。两处无运行时耦合，任一
// 侧单独改名（如 "Kimi Code" → "Kimi"）不会编译报错，会在另一板静默漂移。
// 2026-10-14 徽标统一批将 8 工具名字定案一致，本文件在**测试层**把两源逐键互锁：
// 漂移即红，改名必须两侧同步改（移动侧 codex 与桌面 "Codex"（不带 form 后缀）
// 口径一致，桌面 Codex APP/CLI 的 form 区分不受此锁影响）。
import { describe, expect, it } from "vitest";
import { TOOL_LABELS } from "@/mobile/board-logic";
import { AGENT_BADGE } from "@/lib/agentBadge";

describe("工具显示名双源一致性（TOOL_LABELS ↔ AGENT_BADGE.label）", () => {
  it("各 AgentType 键的名字两处逐一相等（键集派生自编译穷尽的 TOOL_LABELS，漂移即红，改名须双侧同步）", () => {
    // 键集从 Object.keys(TOOL_LABELS) 派生（Record<AgentType,…> 编译穷尽）：
    // 第 9 工具接入时循环自动覆盖新键，硬编码清单会静默逃逸覆盖
    const keys = Object.keys(TOOL_LABELS) as (keyof typeof TOOL_LABELS)[];
    for (const key of keys) {
      expect(TOOL_LABELS[key], `TOOL_LABELS[${key}] 缺失`).toBeTruthy();
      expect(AGENT_BADGE[key], `AGENT_BADGE[${key}] 缺失`).toBeDefined();
      expect(TOOL_LABELS[key]).toBe(AGENT_BADGE[key].label);
    }
  });
});

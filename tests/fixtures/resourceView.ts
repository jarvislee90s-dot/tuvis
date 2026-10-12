// 资源视图测试共用夹具（「按资源」卡片 与 资源大表弹窗两个用例文件）：
// 两边断言的是同一份数据口径（段头计数 / tab 计数 / 行过滤），夹具各自复制一份的话，
// 一旦有人只改一处，两个文件会各绿各的、谁也没错。
import type { SsotResource } from "@/types/extension";

/** 省略字段的 SsotResource 构造器（默认 kind=skill、无启用工具） */
export const res = (o: Partial<SsotResource>): SsotResource => ({
  name: "x",
  kind: "skill",
  enabledTools: [],
  ...o,
});

/** 两段技能 + 各一项 MCP/插件，同时服务两种断言口径：
 *  - alpha 全启用（claude+codex）→ 竖排钮态「全部/禁用」；beta 全未启用 → 「全部/启用」
 *  - 搜 alpha 或 beta 各命中 1 行 → 卡片段头 / 弹窗 tab 计数收敛到 1 */
export const RESOURCES = {
  skills: [
    res({ name: "alpha", enabledTools: ["claude", "codex"] }),
    res({ name: "beta", enabledTools: [] }),
  ],
  mcp: [res({ name: "mcp-x", kind: "mcp", enabledTools: ["claude"] })],
  plugins: [res({ name: "plug-x", kind: "plugin", enabledTools: ["codex"] })],
};

export const TOOLS = [
  {
    id: "claude",
    label: "Claude Code",
    skillToggleSupported: true,
    mcpSupported: true,
    pluginSupported: true,
  },
  {
    id: "codex",
    label: "Codex CLI",
    skillToggleSupported: true,
    mcpSupported: true,
    pluginSupported: true,
  },
];

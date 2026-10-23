import { beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import {
  AGENT_TYPES,
  STATUS_DOT_COLOR,
  STATUS_LABEL_KEYS,
  TOOL_BRAND_COLORS,
  TOOL_BRAND_COLORS_DARK,
  TOOL_LABEL_KEYS,
  applyTransition,
  filterByAgent,
  filterEnabledTools,
  formatRelativeTime,
  formatTransition,
  sortChipsByActivity,
  sortSessions,
  statusLabel,
  toolLabel,
} from "@/mobile/board-logic";
import type { Session, SessionStatus, TransitionEvent } from "@/types/session";

// M5 Task 7：显示文案经 i18n 现取（board-logic 不再内联中文）——本文件的文案断言
// 即被测行为（横幅/相对时长），锁中文值，故统一钉在 zh 下断言
beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

// 会话夹具：仅 board-logic 消费的字体段有语义，其余给合法默认值
function makeSession(
  overrides: Partial<Session> & Pick<Session, "id" | "status" | "lastActivityAt">
): Session {
  return {
    agentType: "claude",
    projectName: "proj",
    projectPath: "/tmp/proj",
    title: null,
    gitBranch: null,
    githubUrl: null,
    lastMessage: null,
    lastMessageRole: null,
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "cli",
    jumpSupported: false,
    unread: false,
    ...overrides,
  };
}

describe("board-logic 排序（等待 → 运行 → 空闲，同级 lastActivityAt 降序）", () => {
  it("等待卡排在运行卡之前，运行卡排在空闲卡之前", () => {
    const t = "2026-09-14T00:00:00Z";
    const input = [
      makeSession({ id: "idle", status: "idle", lastActivityAt: t }),
      makeSession({ id: "processing", status: "processing", lastActivityAt: t }),
      makeSession({ id: "waiting", status: "waiting", lastActivityAt: t }),
      makeSession({ id: "finished", status: "finished", lastActivityAt: t }),
      makeSession({ id: "thinking", status: "thinking", lastActivityAt: t }),
      makeSession({ id: "compacting", status: "compacting", lastActivityAt: t }),
    ];
    const sorted = sortSessions(input);
    expect(sorted.map((s) => s.id)).toEqual([
      "waiting",
      "processing",
      "thinking",
      "compacting",
      "idle",
      "finished",
    ]);
  });

  it("同级内按 lastActivityAt 降序（最近活跃在前）", () => {
    const input = [
      makeSession({ id: "old", status: "waiting", lastActivityAt: "2026-09-14T00:00:00Z" }),
      makeSession({ id: "new", status: "waiting", lastActivityAt: "2026-09-14T01:00:00Z" }),
      makeSession({ id: "mid", status: "waiting", lastActivityAt: "2026-09-14T00:30:00Z" }),
    ];
    expect(sortSessions(input).map((s) => s.id)).toEqual(["new", "mid", "old"]);
  });

  it("异常时间戳（不可解析）排同级末尾，且不改变原数组", () => {
    const input = [
      makeSession({ id: "bad", status: "idle", lastActivityAt: "not-a-date" }),
      makeSession({ id: "good", status: "idle", lastActivityAt: "2026-09-14T00:00:00Z" }),
    ];
    const sorted = sortSessions(input);
    expect(sorted.map((s) => s.id)).toEqual(["good", "bad"]);
    expect(input.map((s) => s.id)).toEqual(["bad", "good"]);
  });
});

describe("board-logic 工具过滤", () => {
  const input = [
    makeSession({
      id: "c",
      agentType: "claude",
      status: "idle",
      lastActivityAt: "2026-09-14T00:00:00Z",
    }),
    makeSession({
      id: "x",
      agentType: "codex",
      status: "idle",
      lastActivityAt: "2026-09-14T00:00:00Z",
    }),
    makeSession({
      id: "c2",
      agentType: "claude",
      status: "waiting",
      lastActivityAt: "2026-09-14T01:00:00Z",
    }),
  ];

  it("按 agentType 过滤", () => {
    expect(filterByAgent(input, "claude").map((s) => s.id)).toEqual(["c", "c2"]);
    expect(filterByAgent(input, "codex").map((s) => s.id)).toEqual(["x"]);
  });

  it("全部（all）原样返回，不过滤", () => {
    expect(filterByAgent(input, "all")).toHaveLength(3);
  });

  it("chips 顺序 = AgentType 八值顺序 + 全部在前", () => {
    expect(AGENT_TYPES).toEqual([
      "claude",
      "codex",
      "opencode",
      "openclaw",
      "kimi",
      "workbuddy",
      "zcode",
      "dsh",
    ]);
  });
});

describe("board-logic 工具显示名（P8c 卡片主行）", () => {
  it("键集穷尽 AgentType 八值（Record 守卫，增删 AgentType 时此断言同步修正）", () => {
    expect(Object.keys(TOOL_LABEL_KEYS).sort()).toEqual([...AGENT_TYPES].sort());
  });

  it("八值显示名与桌面 agentBadge 口径一致（codex 不分 APP/CLI，统一 Codex；M5 Task 7 键化后经 toolLabel 现取）", () => {
    expect(toolLabel("claude")).toBe("Claude");
    expect(toolLabel("codex")).toBe("Codex");
    expect(toolLabel("opencode")).toBe("OpenCode");
    expect(toolLabel("openclaw")).toBe("OpenClaw");
    expect(toolLabel("kimi")).toBe("Kimi Code");
    expect(toolLabel("workbuddy")).toBe("WorkBuddy");
    expect(toolLabel("zcode")).toBe("ZCode");
    expect(toolLabel("dsh")).toBe("DSH");
  });

  it("toolLabel：未知工具 id 回落原文（wire 防御，不渲染 undefined）", () => {
    expect(toolLabel("future-tool")).toBe("future-tool");
  });
});

describe("board-logic 状态 → 三色映射（红=等待 / 黄=运行 / 绿=空闲）", () => {
  const cases: Array<[SessionStatus, string]> = [
    ["waiting", "red"],
    ["processing", "yellow"],
    ["thinking", "yellow"],
    ["compacting", "yellow"],
    ["idle", "green"],
    ["finished", "green"],
  ];
  it.each(cases)("%s → %s", (status, color) => {
    expect(STATUS_DOT_COLOR[status]).toContain(color);
  });
});

// M3 Task 6：跃迁事件 → 展示层纯函数（横幅文案 / 看板卡应用）
describe("board-logic 跃迁展示（M3 Task 6）", () => {
  function makeTransition(overrides: Partial<TransitionEvent> = {}): TransitionEvent {
    return {
      sessionId: "s1",
      agentType: "claude",
      from: "processing",
      to: "waiting",
      projectName: "mam",
      lastMessage: null,
      flapFromSubagentActivity: false,
      ts: 42,
      ...overrides,
    };
  }

  it("STATUS_LABEL_KEYS 键集穷尽 SessionStatus 六值（横幅不会静默缺文案；M5 Task 7 键化，值 = mobile.status.* 键）", () => {
    expect(Object.keys(STATUS_LABEL_KEYS).sort()).toEqual([
      "compacting",
      "finished",
      "idle",
      "processing",
      "thinking",
      "waiting",
    ]);
    // 键必须真实存在于目录（键名拼错 → i18n.t 回落键本身，横幅显示键路径）
    for (const key of Object.values(STATUS_LABEL_KEYS)) {
      expect(i18n.t(key)).not.toBe(key);
    }
    expect(statusLabel("waiting")).toBe("等待操作");
    expect(statusLabel("finished")).toBe("已完成");
    expect(statusLabel("bogus")).toBe("bogus"); // 未知 wire 值回落原文
  });

  it("formatTransition：工具 · 项目 · 前态 → 后态 [· 消息预览]", () => {
    expect(formatTransition(makeTransition())).toBe("Claude · mam · 运行中 → 等待操作");
    expect(formatTransition(makeTransition({ lastMessage: "需要批准" }))).toBe(
      "Claude · mam · 运行中 → 等待操作 · 需要批准"
    );
    // lastMessage 空串与 null 同样省略预览段（不渲染孤立的 ` · `）
    expect(formatTransition(makeTransition({ lastMessage: "" }))).toBe(
      "Claude · mam · 运行中 → 等待操作"
    );
  });

  it("formatTransition：未知 wire 值回落原样字符串（JSON.parse 结果不可信，不渲染 undefined）", () => {
    const weird = makeTransition({
      agentType: "future-tool" as TransitionEvent["agentType"],
      from: "weird" as TransitionEvent["from"],
      to: "state" as TransitionEvent["to"],
    });
    expect(formatTransition(weird)).toBe("future-tool · mam · weird → state");
  });

  it("applyTransition：命中 (工具, 会话 id) 时更新 status/lastMessage，返回新数组且不改入参", () => {
    const before = makeSession({ id: "s1", status: "processing", lastActivityAt: "t" });
    const input = [before];
    const out = applyTransition(input, makeTransition({ lastMessage: "done" }));
    expect(out).not.toBe(input);
    expect(out[0]).not.toBe(before); // 对象也是新引用（不可变更新）
    expect(out[0].status).toBe("waiting");
    expect(out[0].lastMessage).toBe("done");
    expect(input[0].status).toBe("processing"); // 入参未被改
    expect(before.lastMessage).toBeNull();
  });

  it("applyTransition：ev.lastMessage 为 null 时不覆盖既有消息预览（空事件不清空摘要）", () => {
    // 评审 Important：watcher 的跃迁事件可能不带消息（lastMessage=null）——
    // 此时只更新 status，既有预览原样保留；status 仍照常更新
    const before = makeSession({
      id: "s1",
      status: "processing",
      lastMessage: "既有预览",
      lastActivityAt: "t",
    });
    const out = applyTransition([before], makeTransition({ lastMessage: null }));
    expect(out[0].status).toBe("waiting"); // 状态照常更新
    expect(out[0].lastMessage).toBe("既有预览"); // 预览不被 null 抹掉
  });

  it("applyTransition：ev.lastMessage 为非 null 时正常覆盖既有预览（既有口径保持）", () => {
    const before = makeSession({
      id: "s1",
      status: "processing",
      lastMessage: "旧消息",
      lastActivityAt: "t",
    });
    const out = applyTransition([before], makeTransition({ lastMessage: "新消息" }));
    expect(out[0].lastMessage).toBe("新消息");
  });

  it("applyTransition：跨工具撞 id 不误命中（键取 (工具, id) 二元组，同 watcher diff 口径）", () => {
    const input = [
      makeSession({ id: "shared", agentType: "claude", status: "idle", lastActivityAt: "t" }),
      makeSession({ id: "shared", agentType: "codex", status: "idle", lastActivityAt: "t" }),
    ];
    const out = applyTransition(input, makeTransition({ sessionId: "shared", agentType: "codex" }));
    expect(out[0].status).toBe("idle"); // claude 的同名会话不受影响
    expect(out[1].status).toBe("waiting");
  });

  it("applyTransition：未命中（新会话 / 已消失 / 坏状态串）原样返回同一引用（零多余渲染）", () => {
    const input = [makeSession({ id: "s1", status: "idle", lastActivityAt: "t" })];
    // 目标会话不在当前列表（事件早于快照到达 / 卡已消失）
    expect(applyTransition(input, makeTransition({ sessionId: "gone" }))).toBe(input);
    // 未知状态串（wire 损坏）不写入：状态点取色会渲染出 undefined 类名
    const bad = makeTransition({ to: "bogus" as TransitionEvent["to"] });
    expect(applyTransition(input, bad)).toBe(input);
    expect(input[0].status).toBe("idle");
  });
});

// P8d/P8e（M3 Task 3）：受管过滤 + 品牌色 chips + 活跃排序
describe("P8d/P8e board-logic", () => {
  it("品牌色映射覆盖八工具", () => {
    for (const t of [
      "claude",
      "codex",
      "opencode",
      "openclaw",
      "kimi",
      "workbuddy",
      "zcode",
      "dsh",
    ]) {
      expect(TOOL_BRAND_COLORS[t]).toBeTruthy();
    }
  });

  it("品牌色键集穷尽 AgentType 八值（Record 守卫，增删 AgentType 时此断言同步修正）", () => {
    expect(Object.keys(TOOL_BRAND_COLORS).sort()).toEqual([...AGENT_TYPES].sort());
  });

  it("按最新活跃排序（zcode 12:00 最新在前）", () => {
    const sessions = [
      { agentType: "claude", lastActivityAt: "2026-09-15T10:00:00Z" },
      { agentType: "zcode", lastActivityAt: "2026-09-15T12:00:00Z" },
      { agentType: "claude", lastActivityAt: "2026-09-15T11:00:00Z" },
    ];
    const sorted = sortChipsByActivity(["claude", "zcode"], sessions);
    expect(sorted[0]).toBe("zcode"); // 12:00 最新
    expect(sorted[1]).toBe("claude");
  });

  it("无会话工具排末尾（有活跃时间的工具在前，相对顺序稳定）", () => {
    const sessions = [{ agentType: "codex", lastActivityAt: "2026-09-15T08:00:00Z" }];
    const sorted = sortChipsByActivity(["zcode", "codex", "claude"], sessions);
    expect(sorted).toEqual(["codex", "zcode", "claude"]);
  });

  it("不可解析时间戳按 0 处理（NaN 防御：坏时间不压过无会话工具）", () => {
    const sessions = [
      { agentType: "codex", lastActivityAt: "not-a-date" },
      { agentType: "claude", lastActivityAt: "2026-09-15T08:00:00Z" },
    ];
    const sorted = sortChipsByActivity(["codex", "claude", "zcode"], sessions);
    expect(sorted).toEqual(["claude", "codex", "zcode"]);
  });

  it("仅显示受管工具 ∩ 有卡工具", () => {
    const result = filterEnabledTools(["claude", "codex", "zcode"], new Set(["claude", "zcode"]));
    expect(result).toEqual(["claude", "zcode"]);
    expect(result).not.toContain("codex");
  });
});

describe("board-logic 相对时长（now 由调用方注入，可测）", () => {
  const now = Date.parse("2026-09-14T12:00:00Z");

  it("刚发生（<1 分钟）→ 刚刚", () => {
    expect(formatRelativeTime("2026-09-14T11:59:30Z", now)).toBe("刚刚");
  });

  it("小时级 → N 小时前", () => {
    expect(formatRelativeTime("2026-09-14T09:00:00Z", now)).toBe("3 小时前");
  });

  it("天级 → N 天前", () => {
    expect(formatRelativeTime("2026-09-12T00:00:00Z", now)).toBe("2 天前");
  });

  it("分钟级 → N 分钟前", () => {
    expect(formatRelativeTime("2026-09-14T11:33:00Z", now)).toBe("27 分钟前");
  });

  it("未来时间戳（时钟偏差）→ 刚刚", () => {
    expect(formatRelativeTime("2026-09-14T12:05:00Z", now)).toBe("刚刚");
  });

  it("不可解析 → 占位符", () => {
    expect(formatRelativeTime("garbage", now)).toBe("--");
  });
});

// 2026-10-05 UI 改版：chips 中性化后，旧 P8f 压暗方案（CHIP_LIGHT_TEXT_FACTOR /
// darkenHex）退役删除；品牌识别色只保留「卡片檐」装饰用途。本组测试锁台账
// 「颜色矩阵」的檐色两表（白天表 + 夜间提亮表），数值漂移即变红。
describe("台账檐色两表（TOOL_BRAND_COLORS / TOOL_BRAND_COLORS_DARK）", () => {
  it("白天表关键值精确匹配（docs/design/tool-color-ledger.md 颜色矩阵）", () => {
    expect(TOOL_BRAND_COLORS.claude).toBe("#D97757"); // 官方 Crail 橙
    expect(TOOL_BRAND_COLORS.codex).toBe("#10A37F"); // OpenAI 唯一点缀绿
    expect(TOOL_BRAND_COLORS.opencode).toBe("#6B6B6B"); // 终端灰度 → 墨灰
    expect(TOOL_BRAND_COLORS.openclaw).toBe("#D2453C"); // 龙虾猩红
    expect(TOOL_BRAND_COLORS.kimi).toBe("#0B0E1A"); // 蓝黑（不变）
    expect(TOOL_BRAND_COLORS.workbuddy).toBe("#4AD06A"); // 猫绿（不变）
    expect(TOOL_BRAND_COLORS.zcode).toBe("#141413"); // 纯黑（渐变退役）
    expect(TOOL_BRAND_COLORS.dsh).toBe("#4D6BFE"); // DeepSeek 蓝（不变）
  });

  it("键集穷尽 AgentType 八值（Record 守卫，增删 AgentType 时此断言同步修正）", () => {
    expect(Object.keys(TOOL_BRAND_COLORS_DARK).sort()).toEqual([...AGENT_TYPES].sort());
  });

  it("夜间表关键值精确匹配（台账「夜间皮肤处理规则」：黑系提亮为灰阶）", () => {
    expect(TOOL_BRAND_COLORS_DARK.kimi).toBe("#8A94A6"); // 蓝黑 → 蓝灰
    expect(TOOL_BRAND_COLORS_DARK.zcode).toBe("#A39C8C"); // 纯黑 → 暖灰
    expect(TOOL_BRAND_COLORS_DARK.opencode).toBe("#8C8C88"); // 墨灰 → 中灰
    expect(TOOL_BRAND_COLORS_DARK.claude).toBe("#E08A66"); // 橙提亮
    expect(TOOL_BRAND_COLORS_DARK.openclaw).toBe("#E06A5E"); // 猩红提亮
    // 彩色系夜卡底达标的原色不动
    expect(TOOL_BRAND_COLORS_DARK.codex).toBe(TOOL_BRAND_COLORS.codex);
    expect(TOOL_BRAND_COLORS_DARK.workbuddy).toBe(TOOL_BRAND_COLORS.workbuddy);
    expect(TOOL_BRAND_COLORS_DARK.dsh).toBe(TOOL_BRAND_COLORS.dsh);
  });
});

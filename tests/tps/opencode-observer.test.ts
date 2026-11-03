// @vitest-environment node
// OpenCode 观察插件单元测试（2026-10-24 TPS 计划 Task 6，brief Step1 红测试）。
//
// 被测对象：resources/tps/opencode/tuvis-tps-observer.ts（真实宿主 2.0.22 已核对的
// `{ id, setup }` 形态；event.subscribe 返回 AsyncIterable——二进制字符串证据，
// 见 docs/superpowers/evidence 与 task-6-report）。
//
// 隔离纪律（2026-10-07 事故族）：假宿主 harness 全程内存投递，落盘只进
// `os.tmpdir()` 下的临时 home——**绝不**触真机 ~/.config/opencode、~/.opencode、
// ~/.local/share/opencode、~/.tuvis。identity hash 注入使 eventKey = 事件 id 原串
//（生产默认 sha256 全长 hex，不截前 8 位——由专门用例锁定）。
//
// brief Step1 六个红测试用例 + 两条字面 expect：
//   expect(observed.map((e) => e.eventKey)).toEqual(["e1", "e2"]);
//   expect(firstNonEmpty).toBe(2000);

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { describe, expect, it } from "vitest";
// 被测模块：插件源文件本身就是分发产物（嵌入 Tuvis 的同一份字节）
import { createTpsObserver } from "../../resources/tps/opencode/tuvis-tps-observer.ts";
import defaultObserver from "../../resources/tps/opencode/tuvis-tps-observer.ts";

// ==== 假宿主 harness ====

/** 事实文件路径（与插件写盘路径同一推导：home/.tuvis/tps/opencode/facts.jsonl） */
function factsPath(home: string): string {
  return path.join(home, ".tuvis", "tps", "opencode", "facts.jsonl");
}

function makeTempHome(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "tps-opencode-test-"));
}

/** 独立注入的测试依赖：临时 home / 固定时钟 / 即时落盘 / identity hash */
function testDeps(home: string) {
  return {
    homedir: () => home,
    nowMs: () => 1_000_000,
    flushDelayMs: 2,
    hash: (s: string) => s, // identity：eventKey = 事件 id 原串（不截断）
  };
}

/** 事件信封（2.0.22 wire 形态：{id, created, type, data}） */
function ev(
  id: string,
  created: number,
  type: string,
  data: Record<string, unknown>
): Record<string, unknown> {
  return { id, created, type, data };
}

/** 假宿主：subscribe 返回一个**自身没有 next** 的合法 AsyncIterable（acceptance 关卡） */
function fakeHost(events: Array<Record<string, unknown>>) {
  const queue = [...events];
  const stream: Record<symbol, unknown> = {
    [Symbol.asyncIterator]() {
      let i = 0;
      return {
        next: async () =>
          i < queue.length ? { value: queue[i++]!, done: false } : { value: undefined, done: true },
      };
    },
  };
  return {
    event: {
      subscribe: async () => stream,
    },
  };
}

/** 跑完一轮观察并读回事实行（teardown 即冲刷；文件读回为 JSON 行数组） */
async function observe(
  home: string,
  events: Array<Record<string, unknown>>,
  host: ReturnType<typeof fakeHost>,
  depsOverride?: Record<string, unknown>
): Promise<Array<Record<string, unknown>>> {
  const observer = createTpsObserver({ ...testDeps(home), ...depsOverride });
  const teardown = await observer.setup(host as never);
  await new Promise((r) => setTimeout(r, 20)); // 让消费循环吃完全部队列
  await teardown?.();
  const text = fs.readFileSync(factsPath(home), "utf8");
  return text
    .split("\n")
    .filter((l) => l.trim().length > 0)
    .map((l) => JSON.parse(l) as Record<string, unknown>);
}

const S1 = "sess-1";
const M1 = "msg-1";

/** 事实行（剔除 diag:* 诊断行；诊断面由专门用例断言） */
function factRows(rows: Array<Record<string, unknown>>): Array<Record<string, unknown>> {
  return rows.filter((r) => !String(r.type).startsWith("diag:"));
}

// ==== brief Step1 红测试 ====

describe("OpenCode 观察插件（Task 6）", () => {
  it("标准 AsyncIterable 自身无 next 仍能消费，start/delta/end 同键成行", async () => {
    const home = makeTempHome();
    const events = [
      ev("s", 1000, "session.step.started", {
        sessionID: S1,
        assistantMessageID: M1,
        started: 900,
        model: { id: "glm-5.3-flash", providerID: "volcengine" },
      }),
      ev("d1", 2000, "session.text.delta", {
        sessionID: S1,
        assistantMessageID: M1,
        delta: "hello",
      }),
      ev("e", 5000, "session.step.ended", {
        sessionID: S1,
        assistantMessageID: M1,
        finish: "stop",
        tokens: { input: 10, output: 300, reasoning: 0, cache: { read: 0, write: 0 } },
      }),
    ];
    const rows = factRows(await observe(home, events, fakeHost(events)));

    const types = rows.map((r) => r.type);
    expect(types).toEqual(expect.arrayContaining(["start", "delta", "end"]));
    // 同键：三行共享同一 (sessionKey, messageKey)（identity hash → 原串）
    for (const key of ["sessionKey", "messageKey"]) {
      const distinct = new Set(rows.map((r) => r[key]));
      expect(distinct).toEqual(new Set([key === "sessionKey" ? S1 : M1]));
    }
    // start 行带模型身份与起点（started 优先宿主 started 字段）
    const start = rows.find((r) => r.type === "start")!;
    expect(start.model).toBe("glm-5.3-flash");
    expect(start.provider).toBe("volcengine");
    expect(start.started).toBe(900);
    // end 行带结束 tokens（只在 end 行出现，数字面）
    const end = rows.find((r) => r.type === "end")!;
    expect(end.tokens).toBe(300);
    expect(end.finish).toBe("stop");
  });

  it("空 delta 不设 F：首个非空 delta 时刻才成为 firstNonEmpty", async () => {
    const home = makeTempHome();
    const events = [
      ev("s", 1000, "session.step.started", {
        sessionID: S1,
        assistantMessageID: M1,
        model: { id: "m", providerID: "p" },
      }),
      ev("d-empty", 1500, "session.text.delta", {
        sessionID: S1,
        assistantMessageID: M1,
        delta: "",
      }),
      ev("d-nonempty", 2000, "session.text.delta", {
        sessionID: S1,
        assistantMessageID: M1,
        delta: "x",
      }),
    ];
    const rows = factRows(await observe(home, events, fakeHost(events)));
    // 空 delta 行如实记录（hasNonEmptyDelta=false）
    const emptyRow = rows.find((r) => r.eventKey === "d-empty")!;
    expect(emptyRow.hasNonEmptyDelta).toBe(false);
    expect(emptyRow.deltaChars).toBe(0);
    // F 只由首个非空 delta 设立（created=2000）
    const firstNonEmpty = rows.find((r) => r.hasNonEmptyDelta === true)!.created;
    expect(firstNonEmpty).toBe(2000);
  });

  it("重复 event 幂等：同 id 重复投递只记一行", async () => {
    const home = makeTempHome();
    const deltaOf = (id: string) =>
      ev(id, 1000, "session.text.delta", {
        sessionID: S1,
        assistantMessageID: M1,
        delta: "x",
      });
    const events = [deltaOf("e1"), deltaOf("e2"), deltaOf("e1")]; // e1 重复投递
    const rows = await observe(home, events, fakeHost(events));
    const observed = rows.filter((r) => r.type === "delta");
    expect(observed.map((e) => e.eventKey)).toEqual(["e1", "e2"]);
  });

  it("订阅失败有诊断：subscribe 抛错与不可迭代返回值都不静默", async () => {
    const home = makeTempHome();
    // subscribe 抛错 → 诊断行，setup 本身不向宿主抛
    const throwing = {
      event: {
        subscribe: async () => {
          throw new Error("boom");
        },
      },
    };
    const rows1 = await observe(home, [], throwing as never);
    expect(rows1.some((r) => String(r.type).startsWith("diag:"))).toBe(true);

    // 返回值既不可迭代也不是回调 → 诊断行（不猜接口、不静默吞）
    const unsupported = { event: { subscribe: async () => 42 } };
    const rows2 = await observe(home, [], unsupported as never);
    expect(rows2.some((r) => r.type === "diag:stream-unsupported")).toBe(true);
  });

  it("两会话/两 message 交错不串键：行键与 ordinal 各自独立", async () => {
    const home = makeTempHome();
    const step = (id: string, sid: string, mid: string, created: number, delta?: string) =>
      delta === undefined
        ? ev(id, created, "session.step.started", {
            sessionID: sid,
            assistantMessageID: mid,
            model: { id: "m", providerID: "p" },
          })
        : ev(id, created, "session.text.delta", {
            sessionID: sid,
            assistantMessageID: mid,
            delta,
          });
    const events = [
      step("a1", "s1", "m1", 100),
      step("b1", "s2", "m2", 110),
      step("a2", "s1", "m1", 120, "x"),
      step("b2", "s2", "m2", 130, "y"),
      step("a3", "s1", "m1", 140, "z"),
    ];
    const rows = factRows(await observe(home, events, fakeHost(events)));
    expect(rows).toHaveLength(5);
    // 每行键与自身事件一致（交错不串）
    for (const r of rows) {
      const data = r.eventKey as string;
      const sid = data.startsWith("a") ? "s1" : "s2";
      const mid = data.startsWith("a") ? "m1" : "m2";
      expect(r.sessionKey).toBe(sid);
      expect(r.messageKey).toBe(mid);
    }
    // ordinal 按键独立递增（s1/m1: 1,2,3；s2/m2: 1,2）
    const ord = (id: string) => rows.find((r) => r.eventKey === id)!.ordinal;
    expect([ord("a1"), ord("a2"), ord("a3")]).toEqual([1, 2, 3]);
    expect([ord("b1"), ord("b2")]).toEqual([1, 2]);
  });

  it("累计 usage 不再累加：usage 只取自 step.ended 自身，不消费累计面", async () => {
    const home = makeTempHome();
    const events = [
      ev("s1", 1000, "session.step.started", {
        sessionID: S1,
        assistantMessageID: M1,
        model: { id: "m", providerID: "p" },
      }),
      ev("e1", 2000, "session.step.ended", {
        sessionID: S1,
        assistantMessageID: M1,
        finish: "tool-calls",
        tokens: { input: 10, output: 100, reasoning: 0, cache: { read: 0, write: 0 } },
      }),
      ev("s2", 3000, "session.step.started", {
        sessionID: S1,
        assistantMessageID: M1,
        model: { id: "m", providerID: "p" },
      }),
      ev("e2", 4000, "session.step.ended", {
        sessionID: S1,
        assistantMessageID: M1,
        finish: "stop",
        tokens: { input: 10, output: 150, reasoning: 0, cache: { read: 0, write: 0 } },
      }),
      // 累计面（会话级 usage 快照）永不入行——防止重复入账
      ev("u", 4500, "session.usage.updated", {
        sessionID: S1,
        cost: 1,
        tokens: { input: 20, output: 250, reasoning: 0, cache: { read: 0, write: 0 } },
      }),
    ];
    const rows = factRows(await observe(home, events, fakeHost(events)));
    const endRows = rows.filter((r) => r.type === "end");
    // 两步各自成行，tokens 是各自事件自带的数值（插件从不安加）
    expect(endRows.map((r) => r.tokens)).toEqual([100, 150]);
    // 累计面无行（不消费 session.usage.updated / message.updated）
    expect(rows.some((r) => r.eventKey === "u")).toBe(false);
  });

  it("不保存正文：delta 内容测长即弃，落盘字节无正文", async () => {
    const home = makeTempHome();
    const secret = "SECRET-CONTENT-必须-不落盘";
    const events = [
      ev("d", 1000, "session.text.delta", {
        sessionID: S1,
        assistantMessageID: M1,
        delta: secret,
      }),
    ];
    const rows = factRows(await observe(home, events, fakeHost(events)));
    expect(rows[0]!.deltaChars).toBe(secret.length);
    expect(rows[0]!.hasNonEmptyDelta).toBe(true);
    const raw = fs.readFileSync(factsPath(home), "utf8");
    expect(raw.includes("SECRET-CONTENT")).toBe(false);
  });

  it("retry 事件入行（attempt 字段），step.failed 的中止形态单列 cancelled", async () => {
    const home = makeTempHome();
    const events = [
      ev("s", 1000, "session.step.started", {
        sessionID: S1,
        assistantMessageID: M1,
        model: { id: "m", providerID: "p" },
      }),
      ev("r", 1500, "session.retry.scheduled", {
        sessionID: S1,
        assistantMessageID: M1,
        attempt: 2,
        at: 1600,
      }),
      ev("f", 5000, "session.step.failed", {
        sessionID: S1,
        assistantMessageID: M1,
        error: { name: "MessageAbortedError", data: { message: "interrupted" } },
      }),
    ];
    const rows = factRows(await observe(home, events, fakeHost(events)));
    const retry = rows.find((r) => r.type === "retry")!;
    expect(retry.attempt).toBe(2);
    // 中止 = 用户取消形态 → type=cancelled（outcome 面留给 Rust 读取器映射）
    const failed = rows.find((r) => r.type === "cancelled")!;
    expect(failed.messageKey).toBe(M1);
  });

  it("生产默认 hash = sha256 全长 hex（不截前 8 位）且默认导出即宿主形态", async () => {
    expect(defaultObserver.id).toBe("tuvis-tps-observer");
    expect(typeof defaultObserver.setup).toBe("function");
    // 默认依赖（真实 homedir/sha256）不落盘验证——只验证 hash 形态：
    // 用 factory 传真实 hash 函数族 + 临时 home（不触真机目录）
    const home = makeTempHome();
    // 默认 hash（sha256）+ 临时 home：只省略 deps.hash，其余照旧注入
    const events = [
      ev("only", 1000, "session.text.delta", {
        sessionID: S1,
        assistantMessageID: M1,
        delta: "x",
      }),
    ];
    const rows = factRows(
      await observe(home, events, fakeHost(events), {
        hash: undefined,
      })
    );
    const key = String(rows[0]!.eventKey);
    expect(key).toMatch(/^[0-9a-f]{64}$/); // 全长 64 hex
    expect(key.length).toBeGreaterThan(8); // 不是前 8 位截断
  });
});

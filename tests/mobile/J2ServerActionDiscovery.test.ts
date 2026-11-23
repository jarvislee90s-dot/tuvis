// J2（无本地锚发现）单测：服务端动作清单的 kind 映射与本地锚去重合并。
// 纯函数层（opencodeActionAnchors.mergeServerActionAnchors）——面板接线（sync
// 合并/核对按钮）由既有 OpenCodeNativeDecisionAnchor / 决策锚测试覆盖的同一渲染
// 路径消费（anchorRows 行形态不变）。
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  mergeServerActionAnchors,
  resetActionAnchorMemoryForTests,
  serverOperationToAnchorKind,
  addActionAnchor,
  readActionAnchors,
  type OpenCodeActionAnchor,
} from "../../src/mobile/opencodeActionAnchors";

const SESSION = "ses_j2_merge";

function local(
  actionId: string,
  kind: OpenCodeActionAnchor["kind"],
  at: number
): OpenCodeActionAnchor {
  return { actionId, kind, targetId: "t", at };
}

describe("J2 server action discovery", () => {
  beforeEach(() => {
    resetActionAnchorMemoryForTests();
  });
  afterEach(() => {
    resetActionAnchorMemoryForTests();
  });

  it("serverOperationToAnchorKind 映射已知操作、未知/暂停形态跳过", () => {
    expect(serverOperationToAnchorKind("send")).toBe("send");
    expect(serverOperationToAnchorKind("answer")).toBe("answer");
    expect(serverOperationToAnchorKind("decide")).toBe("decide");
    expect(serverOperationToAnchorKind("stop_session")).toBe("stop");
    expect(serverOperationToAnchorKind("set_agent")).toBe("set_agent");
    expect(serverOperationToAnchorKind("retract_outbox")).toBe("retract");
    expect(serverOperationToAnchorKind("set_outbox_paused")).toBeNull();
    expect(serverOperationToAnchorKind("unknown")).toBeNull();
  });

  it("合并：本地锚优先，服务端只补充本地缺失项（action ID 去重）", () => {
    const locals = [local("act-local", "send", 100)];
    const server = [
      { actionId: "act-local", operation: "send" },
      { actionId: "act-server-1", operation: "answer" },
    ];
    const merged = mergeServerActionAnchors(locals, server);
    expect(merged.map((row) => row.actionId)).toEqual(["act-local", "act-server-1"]);
    expect(merged[0]).toEqual(local("act-local", "send", 100));
  });

  it("本地锚清空后服务端清单仍提供核对入口（清空存储找回）", () => {
    const server = [
      { actionId: "act-server-1", operation: "send" },
      { actionId: "act-server-2", operation: "stop_session" },
    ];
    const merged = mergeServerActionAnchors([], server);
    expect(merged.map((row) => row.actionId)).toEqual(["act-server-1", "act-server-2"]);
    expect(merged[0].kind).toBe("send");
    expect(merged[1].kind).toBe("stop");
  });

  it("无法映射 kind 的行跳过（set_outbox_paused / unknown），不动锚语义", () => {
    const server = [
      { actionId: "act-pause", operation: "set_outbox_paused" },
      { actionId: "act-unknown", operation: "unknown" },
      { actionId: "act-ok", operation: "decide" },
    ];
    const merged = mergeServerActionAnchors([], server);
    expect(merged.map((row) => row.actionId)).toEqual(["act-ok"]);
  });

  it("合并结果上限沿用 MAX_ACTION_ANCHORS（截断最旧，服务端清单仍可按行核对）", () => {
    const locals: OpenCodeActionAnchor[] = [];
    const server = Array.from({ length: 30 }, (_, i) => ({
      actionId: `act-srv-${i}`,
      operation: "send",
    }));
    const merged = mergeServerActionAnchors(locals, server);
    expect(merged.length).toBeLessThanOrEqual(20);
  });

  it("与既有锚存储互通：合并行可被 addActionAnchor 语义收敛（去重后本地 wins）", () => {
    addActionAnchor(SESSION, { actionId: "act-x", kind: "send", targetId: "t" }, 1);
    const stored = readActionAnchors(SESSION);
    const merged = mergeServerActionAnchors(stored, [{ actionId: "act-x", operation: "send" }]);
    expect(merged.length).toBe(1);
    expect(merged[0].targetId).toBe("t");
  });
});

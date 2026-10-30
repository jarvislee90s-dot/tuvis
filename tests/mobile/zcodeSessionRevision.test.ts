// F1 纯函数层：同会话同代的 revision 单调，以及「落地语义」的分流与合并。
//
// 锁五件事：
// 1. 慢 GET 的旧 revision 不得覆盖先到的 SSE 新 revision；
// 2. 跨会话/换代不误挡；
// 3. 更旧的 `ready=false` 只能**安全降级**（关写面），内容不得整份回退；
// 4. 更旧的 `recovery` 只能**收窄能力**，不得开机能力、不得复活旧内容；
// 5. 同 revision 允许完整落地（权威 GET 恢复能力矩阵），当前无锚（revision=null）不判旧。
import { describe, expect, it } from "vitest";
import {
  degradedCapabilities,
  mergeDegradedView,
  mergeRecoveredCaps,
  resolveSessionViewLanding,
  shouldApplySessionView,
  type AppliedSessionView,
} from "@/mobile/useZcodeControl";
import type { CapabilityView, QueueView, SessionView } from "@/lib/api/zcodeControl";

function applied(overrides: Partial<AppliedSessionView> = {}): AppliedSessionView {
  return {
    sessionId: "sess_a",
    generation: 3,
    revision: 12,
    ready: true,
    ...overrides,
  };
}

const openFlag = { enabled: true, reasonCode: null };
const closedFlag = (reasonCode: string | null = "needs_evidence") => ({
  enabled: false,
  reasonCode,
});

function openCapabilities(): CapabilityView {
  return {
    send: openFlag,
    singleSelect: openFlag,
    multiSelect: openFlag,
    freeText: openFlag,
    interrupt: openFlag,
    approvalReply: openFlag,
    model: openFlag,
    thoughtLevel: openFlag,
  };
}

function closedCapabilities(): CapabilityView {
  return {
    send: closedFlag(),
    singleSelect: closedFlag(),
    multiSelect: closedFlag(),
    freeText: closedFlag(),
    interrupt: closedFlag(),
    approvalReply: closedFlag(),
    model: closedFlag(),
    thoughtLevel: closedFlag(),
  };
}

function queue(ids: string[]): QueueView {
  return {
    known: true,
    paused: false,
    items: ids.map((id, position) => ({ inputId: id, text: id, position })),
  };
}

function sessionView(overrides: Partial<SessionView> = {}): SessionView {
  return {
    sessionId: "sess_a",
    generation: 3,
    revision: 12,
    ready: true,
    pending: [],
    control: { activeWorks: [] },
    selection: null,
    capabilities: openCapabilities(),
    queue: queue([]),
    ...overrides,
  };
}

describe("shouldApplySessionView：同代 revision 单调", () => {
  it("sse_rev12_then_get_rev11_is_rejected", () => {
    expect(shouldApplySessionView(applied(), applied({ revision: 11 }), "get")).toBe(false);
  });

  it("get_rev12_then_sse_rev12_is_allowed", () => {
    expect(shouldApplySessionView(applied(), applied({ revision: 12 }), "sse")).toBe(true);
  });

  it("ready_true_with_null_revision_cannot_override_numeric_revision", () => {
    expect(shouldApplySessionView(applied(), applied({ revision: null }), "get")).toBe(false);
    expect(shouldApplySessionView(applied(), applied({ revision: null }), "sse")).toBe(false);
  });

  it("safety_degrade_without_revision_is_always_applied", () => {
    expect(
      shouldApplySessionView(applied(), applied({ ready: false, revision: null }), "safety-degrade")
    ).toBe(true);
    // 即使来源标记普通 GET，ready=false 也必须能关写面。
    expect(shouldApplySessionView(applied(), applied({ ready: false, revision: 9 }), "get")).toBe(
      true
    );
  });

  it("cross_session_and_newer_generation_are_not_blocked", () => {
    expect(
      shouldApplySessionView(applied(), applied({ sessionId: "sess_b", revision: 1 }), "sse")
    ).toBe(true);
    expect(shouldApplySessionView(applied(), applied({ generation: 4, revision: 1 }), "sse")).toBe(
      true
    );
    expect(shouldApplySessionView(applied(), applied({ generation: 2, revision: 99 }), "sse")).toBe(
      false
    );
  });

  it("recovery_read_can_restore_capabilities_behind_a_newer_event", () => {
    // 更旧的权威现读仍会落地（能力矩阵可收窄），但不再整份覆盖。
    expect(shouldApplySessionView(applied(), applied({ revision: 11 }), "recovery")).toBe(true);
  });

  it("degraded_current_view_does_not_accept_older_ready_content", () => {
    // 当前已降级（ready=false）不等于放弃时序锚：更旧的 ready=true 快照仍不得
    // 复活旧业务内容（原「降级中一律放行」会让 rev11 覆盖 rev12 的队列）。
    expect(
      shouldApplySessionView(applied({ ready: false }), applied({ revision: 11 }), "get")
    ).toBe(false);
  });
});

describe("resolveSessionViewLanding：三种落地语义分流", () => {
  it("newer_or_equal_view_lands_whole", () => {
    expect(resolveSessionViewLanding(null, applied(), "get")).toBe("replace");
    expect(resolveSessionViewLanding(applied(), applied({ revision: 12 }), "sse")).toBe("replace");
    expect(resolveSessionViewLanding(applied(), applied({ revision: 13 }), "sse")).toBe("replace");
    expect(resolveSessionViewLanding(applied(), applied({ generation: 4 }), "sse")).toBe("replace");
    expect(resolveSessionViewLanding(applied(), applied({ sessionId: "sess_b" }), "sse")).toBe(
      "replace"
    );
    // 当前无锚（revision=null）：无可比较，按完整落地。
    expect(
      resolveSessionViewLanding(applied({ revision: null }), applied({ revision: 1 }), "sse")
    ).toBe("replace");
  });

  it("stale_safety_degrade_lands_as_degrade_only", () => {
    // 更旧的 ready=false：关写面但绝不整份覆盖（GET 来源也一样）。
    expect(
      resolveSessionViewLanding(applied(), applied({ revision: 11, ready: false }), "get")
    ).toBe("degrade");
    expect(
      resolveSessionViewLanding(applied(), applied({ revision: 11, ready: false }), "sse")
    ).toBe("degrade");
    expect(
      resolveSessionViewLanding(
        applied(),
        applied({ revision: null, ready: false }),
        "safety-degrade"
      )
    ).toBe("degrade");
    // 换代的降级仍是完整落地：新代次的内容就是权威内容。
    expect(
      resolveSessionViewLanding(applied(), applied({ generation: 4, ready: false }), "sse")
    ).toBe("replace");
  });

  it("stale_recovery_lands_as_caps_only", () => {
    expect(resolveSessionViewLanding(applied(), applied({ revision: 11 }), "recovery")).toBe(
      "caps"
    );
    // 非 recovery 来源的更旧 ready=true 快照：直接丢弃（旧内容不得回归）。
    expect(resolveSessionViewLanding(applied(), applied({ revision: 11 }), "get")).toBe("drop");
    expect(resolveSessionViewLanding(applied(), applied({ revision: 11 }), "sse")).toBe("drop");
  });

  it("older_generation_is_dropped", () => {
    expect(resolveSessionViewLanding(applied(), applied({ generation: 2 }), "recovery")).toBe(
      "drop"
    );
  });
});

describe("mergeDegradedView：内容不倒退，只关写面", () => {
  it("stale_degrade_keeps_newer_content_and_closes_writes", () => {
    const current = sessionView({
      revision: 12,
      ready: true,
      pending: [
        {
          interactionId: "pi_1",
          kind: "permission",
          anchorRowId: 7,
          toolCallId: "tc_1",
          createdAt: null,
          summary: "允许执行",
          toolName: "Bash",
          schemaInteraction: null,
          sensitive: false,
          autoResolution: false,
          autoResolutionState: null,
          questions: [],
          options: [],
          detail: null,
        },
      ],
      queue: queue(["queue_a", "queue_b"]),
    });
    const stale = sessionView({
      revision: 11,
      ready: false,
      pending: [],
      queue: queue([]),
      capabilities: closedCapabilities(),
    });

    const merged = mergeDegradedView(current, stale);

    expect(merged.ready).toBe(false);
    expect(merged.revision).toBe(12);
    expect(merged.queue?.items.map((item) => item.inputId)).toEqual(["queue_a", "queue_b"]);
    expect(merged.pending.map((item) => item.interactionId)).toEqual(["pi_1"]);
    expect(merged.capabilities).toEqual(degradedCapabilities());
  });

  it("degrade_never_opens_writes_from_a_ready_incoming", () => {
    // 异常路径：incoming.ready=true 也绝不由降级合并开写面。
    const current = sessionView({ ready: false, capabilities: closedCapabilities() });
    const merged = mergeDegradedView(current, sessionView({ ready: true, revision: 9 }));
    expect(merged.ready).toBe(false);
    expect(merged.capabilities).toEqual(degradedCapabilities());
  });
});

describe("mergeRecoveredCaps：只收窄，不替换内容", () => {
  it("recovery_merge_only_tightens_capabilities", () => {
    const current = sessionView({
      revision: 12,
      capabilities: {
        ...openCapabilities(),
        multiSelect: closedFlag("needs_evidence"),
      },
      queue: queue(["queue_new"]),
    });
    const older = sessionView({
      revision: 11,
      capabilities: {
        ...openCapabilities(),
        send: closedFlag("not_ready"),
      },
      queue: queue(["queue_old"]),
    });

    const merged = mergeRecoveredCaps(current, older);

    // 内容/时序锚不变：旧队列不得复活。
    expect(merged.revision).toBe(12);
    expect(merged.queue?.items.map((item) => item.inputId)).toEqual(["queue_new"]);
    // 能力只允许由开改关：send 关掉（带本次原因码），其余保持。
    expect(merged.capabilities.send).toEqual({ enabled: false, reasonCode: "not_ready" });
    expect(merged.capabilities.singleSelect.enabled).toBe(true);
    // 已关的能力不得由更旧视图开回来。
    expect(merged.capabilities.multiSelect).toEqual({
      enabled: false,
      reasonCode: "needs_evidence",
    });
  });

  it("recovery_merge_never_enables_a_closed_capability", () => {
    const current = sessionView({ capabilities: closedCapabilities() });
    const merged = mergeRecoveredCaps(
      current,
      sessionView({ revision: 11, capabilities: openCapabilities() })
    );
    expect(merged.capabilities).toEqual(closedCapabilities());
  });
});

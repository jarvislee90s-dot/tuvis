// R07（D3）：前端**展示缓存**按 `capabilityIdentity` 沿用上次支持事实。
//
// 纪律（与后端同一语义，纯展示）：
// - 同一完整目标（`capabilityIdentity` 相同）在连接 stale / 重新识别期间沿用上次
//   已支持的展示，并显示「连接恢复中」；
// - identity 变化（新 build/SHA/规则）或缺失 → **立即清缓存**，不继承旧目标能力；
// - 只替换 `support` 展示字段；`allowed`/`evidence`/`reasonCode` 一字不动
//   （展示缓存不是授权缓存）。
import { describe, expect, it } from "vitest";
import {
  retainCapabilityDisplay,
  type CapabilityDisplayCache,
} from "@/mobile/codexCapabilityDisplay";
import type { CodexEffectiveOperations, CodexSupportState } from "@/mobile/codexIpcApi";

function status(
  over: Partial<NonNullable<CodexEffectiveOperations["operations"][string]>> = {}
): NonNullable<CodexEffectiveOperations["operations"][string]> & { supportInherited?: boolean } {
  return {
    implemented: true,
    allowed: true,
    requiresAutoAdapt: true,
    evidence: "contract_matched",
    support: "supported",
    reasonCode: null,
    ...over,
  };
}

function operations(
  identity: string | undefined,
  ops: CodexEffectiveOperations["operations"]
): CodexEffectiveOperations {
  return {
    policyRevision: 1,
    operations: ops,
    ...(identity === undefined ? {} : { capabilityIdentity: identity }),
  };
}

const supportedOps = operations("cap-1", {
  async_answer_idle: status(),
  blocking_input: status({ evidence: "shipped", requiresAutoAdapt: false }),
});

/** 重连窗内的真实投影：证据槽失配 → 全部回 identifying、allowed=false。 */
const reconnectingOps = operations("cap-1", {
  async_answer_idle: status({
    allowed: false,
    evidence: "none",
    support: "identifying",
    reasonCode: "operation_no_evidence",
  }),
  blocking_input: status({
    allowed: false,
    evidence: "none",
    support: "identifying",
    reasonCode: "operation_no_evidence",
  }),
});

describe("R07 展示缓存（identity 键控）", () => {
  it("r07_same_identity_reconnect_keeps_support_and_flags_reconnecting", () => {
    // 首次：把已支持事实收进缓存。
    const first = retainCapabilityDisplay(null, supportedOps, { stale: false });
    expect(first.retained).toBe(false);
    expect(first.reconnecting).toBe(false);
    expect(first.cache?.identity).toBe("cap-1");

    // 重连：同 identity，真实投影回 identifying + stale → 沿用上次展示。
    const during = retainCapabilityDisplay(first.cache, reconnectingOps, { stale: true });
    expect(during.retained).toBe(true);
    expect(during.reconnecting).toBe(true);
    const idle = during.operations!.operations.async_answer_idle!;
    expect(idle.support).toBe("supported");
    // 展示缓存**不改**授权面。
    expect(idle.allowed).toBe(false);
    expect(idle.evidence).toBe("none");
    expect(idle.reasonCode).toBe("operation_no_evidence");
    expect(during.operations!.capabilityIdentity).toBe("cap-1");
  });

  it("r07_identity_change_clears_cache_and_does_not_inherit", () => {
    const first = retainCapabilityDisplay(null, supportedOps, { stale: false });
    // A→B：identity 变 → 不沿用（缓存被换成新身份的现状）。
    const swapped = operations("cap-2", {
      async_answer_idle: status({
        allowed: false,
        evidence: "none",
        support: "identifying",
        reasonCode: "operation_no_evidence",
      }),
    });
    const after = retainCapabilityDisplay(first.cache, swapped, { stale: true });
    expect(after.retained).toBe(false);
    expect(after.reconnecting).toBe(false);
    expect(after.operations!.operations.async_answer_idle!.support).toBe("identifying");
    expect(after.cache?.identity).toBe("cap-2");
  });

  it("r07_missing_identity_clears_cache", () => {
    const first = retainCapabilityDisplay(null, supportedOps, { stale: false });
    const noIdentity = operations(undefined, {
      async_answer_idle: status({ support: "identifying", evidence: "none", allowed: false }),
    });
    const after = retainCapabilityDisplay(first.cache, noIdentity, { stale: true });
    expect(after.cache).toBeNull();
    expect(after.retained).toBe(false);
    expect(after.operations!.operations.async_answer_idle!.support).toBe("identifying");
  });

  it("r07_real_fact_wins_over_display_cache", () => {
    const first = retainCapabilityDisplay(null, supportedOps, { stale: false });
    // 重新识别完成：real unmatched 优先于缓存里的 supported。
    const real = operations("cap-1", {
      async_answer_idle: status({
        allowed: false,
        evidence: "none",
        support: "unmatched",
        reasonCode: "operation_contract_unmatched",
      }),
    });
    const after = retainCapabilityDisplay(first.cache, real, { stale: false });
    expect(after.retained).toBe(false);
    expect(after.operations!.operations.async_answer_idle!.support).toBe("unmatched");
    // 缓存更新为实时事实（后续同身份重连按 unmatched 沿用）。
    const support = after.cache!.support.async_answer_idle as CodexSupportState;
    expect(support).toBe("unmatched");
  });

  it("r07_undefined_operations_is_passthrough", () => {
    const first = retainCapabilityDisplay(null, supportedOps, { stale: false });
    const after = retainCapabilityDisplay(first.cache, undefined, { stale: true });
    expect(after.operations).toBeUndefined();
    expect(after.retained).toBe(false);
    expect(after.cache).toEqual(first.cache);
  });

  it("r07_reconnecting_flag_requires_stale", () => {
    const first = retainCapabilityDisplay(null, supportedOps, { stale: false });
    const during = retainCapabilityDisplay(first.cache, reconnectingOps, { stale: false });
    expect(during.retained).toBe(true);
    expect(during.reconnecting).toBe(false, "连接未 stale 时不谎报恢复中");
  });

  it("r07_cache_never_makes_unimplemented_look_supported", () => {
    const first = retainCapabilityDisplay(null, supportedOps, { stale: false });
    const withUnimplemented = operations("cap-1", {
      request_cancel: status({ implemented: false, support: "not_implemented", allowed: false }),
    });
    const during = retainCapabilityDisplay(first.cache, withUnimplemented, { stale: true });
    expect(during.operations!.operations.request_cancel!.support).toBe("not_implemented");
    // 缓存不注入「未出现过的操作」。
    expect(during.operations!.operations.async_answer_idle).toBeUndefined();
  });

  it("r07_cache_type_is_plain_data", () => {
    const cache: CapabilityDisplayCache = { identity: "cap-1", support: {} };
    expect(cache.identity).toBe("cap-1");
  });
});

describe("R07 修复轮 1：supportInherited（生产路径可达的「连接恢复中」）", () => {
  it("r1_backend_support_inherited_flag_drives_reconnecting", () => {
    // 生产真实形态：后端已把 support 替换成 supported 并置 supportInherited=true。
    const inheritedOps = operations("cap-1", {
      async_answer_idle: status({ allowed: false, support: "supported", supportInherited: true }),
    });
    const result = retainCapabilityDisplay(null, inheritedOps, { stale: true });
    expect(result.reconnecting).toBe(true, "后端沿用标记必须点亮「连接恢复中」");
    expect(result.operations!.operations.async_answer_idle!.support).toBe("supported");
    // 标记不被当作"实时事实"进缓存（缓存只记 support 结论）。
    expect(result.cache?.support.async_answer_idle).toBe("supported");
  });

  it("r1_backend_inherited_flag_absent_means_no_reconnecting", () => {
    const live = operations("cap-1", {
      async_answer_idle: status({ allowed: true, support: "supported" }),
    });
    const result = retainCapabilityDisplay(null, live, { stale: true });
    expect(result.reconnecting).toBe(false, "实时事实 + 无沿用标记 → 不谎报恢复中");
  });

  it("r1_backend_inherited_flag_marks_even_without_frontend_cache", () => {
    // 首帧就是沿用态（前端还没有缓存）：仍必须显示恢复中。
    const inheritedOps = operations("cap-1", {
      async_answer_idle: status({ allowed: false, support: "supported", supportInherited: true }),
    });
    const result = retainCapabilityDisplay(null, inheritedOps, { stale: false });
    expect(result.reconnecting).toBe(true, "沿用标记不依赖前端缓存与 stale 标志");
  });
});

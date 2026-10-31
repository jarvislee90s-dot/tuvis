import { describe, expect, it } from "vitest";
import type { CapabilitySet, ProjectionVersion, ThreadProjection } from "@/mobile/codexIpcApi";
import {
  shouldApplyProjection,
  type ProjectionApplyContext,
} from "@/mobile/codex-projection-order";

// T02：投影落地守卫的纯比较锁。服务端事实模型 = src-tauri/src/remote/codex_ipc.rs
// `supersedes`（epoch > generation > stale/revision > local_seq）；本模块把同一
// 比较搬到 TS 再加 instance 维度（跨 hub 世系不可直接比较，只有当前代际最新
// 握手有权切换 instance）。GET／SSE／轮询／手动刷新共用这一套守卫。
//
// 夹具纪律：投影本体的 `revision` 与版本束同源（服务端同一字段盖章两处），
// 夹具必须一并设置——只在单侧改会让比较轴失真。

function caps(over: Partial<CapabilitySet> = {}): CapabilitySet {
  return {
    send: true,
    state: true,
    command: true,
    permissions: true,
    blockingInput: true,
    interrupt: false,
    ...over,
  };
}

function ver(over: Partial<ProjectionVersion> = {}): ProjectionVersion {
  return {
    instanceId: "inst-1",
    ownerEpoch: 1,
    ownerGeneration: 1,
    revision: 12,
    localSeq: 12,
    ...over,
  };
}

/** 构造投影：`revision`/`localSeq` 同时写入本体与版本束（线上同源）。 */
function frameAt(
  revision: number | null,
  localSeq: number,
  over: Partial<ThreadProjection> = {}
): ThreadProjection {
  return {
    threadId: "th-1",
    revision,
    stale: false,
    availability: "controllable",
    capabilities: caps(),
    runtimeStatus: "idle",
    contentState: "ready",
    messages: [],
    truncated: false,
    historyToken: "v1-tok1",
    pending: [],
    projectionVersion: ver({ revision, localSeq }),
    ...over,
  };
}

/** 去掉版本束（旧载荷形态）。 */
function legacy(f: ThreadProjection): ThreadProjection {
  const { projectionVersion: _drop, ...rest } = f;
  return rest;
}

/** 简报固定夹具：newFrame 与 oldFrame 同 instance／owner／historyToken，seq 12 vs 11。 */
const oldFrame = frameAt(11, 11);
const newFrame = frameAt(12, 12);

const CONTEXT: ProjectionApplyContext = {
  sessionId: "s1",
  subscriptionGeneration: 3,
  responseSubscriptionGeneration: 3,
  allowInstanceChange: false,
};

describe("codex-projection-order：同 instance 服务端 supersedes 搬运", () => {
  it("fixed_fixture_new_over_old_only：同 instance/owner/historyToken（seq 12 vs 11）只允许新帧落地", () => {
    expect(shouldApplyProjection(newFrame, oldFrame, CONTEXT)).toBe(false);
    expect(shouldApplyProjection(oldFrame, newFrame, CONTEXT)).toBe(true);
  });

  it("same_revision_higher_local_seq_updates：同 revision 更高 localSeq 的开关/暂停帧可更新（终审 F5）", () => {
    // 本地策略更新（队列暂停/探测重发布）不动 Desktop revision，靠 local_seq 决胜
    expect(shouldApplyProjection(newFrame, frameAt(12, 13, { queuePaused: true }), CONTEXT)).toBe(
      true
    );
    // 对称面：旧序号不得反超（不回退）
    expect(shouldApplyProjection(newFrame, frameAt(12, 11, { queuePaused: true }), CONTEXT)).toBe(
      false
    );
  });

  it("t03_same_revision_policy_and_active_turn_frames_land：同 revision 的策略帧（资格翻转/活动回合）经 localSeq 轴可达", () => {
    // T03 的核心场景本身就是「同 revision 策略帧」：idle→active 翻转不改 Desktop
    // revision，只换 local_seq——support 恒 supported、allowed/reason 翻转的帧必须
    // 可达（否则前端永远停在旧资格上，支持展示也会跟着卡死）。
    const idlePolicy = frameAt(12, 12, {
      effectiveOperations: {
        policyRevision: 7,
        capabilityIdentity: "cap-id-1",
        operations: {
          async_answer_idle: {
            implemented: true,
            allowed: true,
            requiresAutoAdapt: true,
            evidence: "contract_matched",
            support: "supported",
            reasonCode: null,
          },
        },
      },
    });
    const activePolicy = frameAt(12, 13, {
      effectiveOperations: {
        policyRevision: 8,
        capabilityIdentity: "cap-id-1",
        operations: {
          async_answer_idle: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: true,
            evidence: "contract_matched",
            support: "supported",
            reasonCode: "operation_runtime_active",
          },
        },
      },
      activeTurnId: "turn-9",
    });
    expect(shouldApplyProjection(idlePolicy, activePolicy, CONTEXT)).toBe(true);
    // 同 revision 旧序号的策略帧不得反超（不回退到旧资格）。
    expect(shouldApplyProjection(activePolicy, idlePolicy, CONTEXT)).toBe(false);
  });

  it("higher_revision_wins_over_local_seq：revision 竞争优先于本地序号", () => {
    expect(shouldApplyProjection(frameAt(11, 99), frameAt(12, 1), CONTEXT)).toBe(true);
  });

  it("owner_epoch_then_generation_decide：旧 owner 的高 revision 拒绝；新 owner 即使 revision 变小也生效", () => {
    const snapAt = (epoch: number, gen: number, rev: number) =>
      frameAt(rev, rev, {
        projectionVersion: ver({
          ownerEpoch: epoch,
          ownerGeneration: gen,
          revision: rev,
          localSeq: rev,
        }),
      });
    const snap = snapAt(2, 3, 50);
    // 旧 epoch：不是新世界
    expect(shouldApplyProjection(snap, snapAt(1, 9, 99), CONTEXT)).toBe(false);
    // 同 epoch 新一代 generation 即使 revision 更低也是合法新状态
    expect(shouldApplyProjection(snap, snapAt(2, 4, 1), CONTEXT)).toBe(true);
    // 新 epoch 本身更新（即使 revision 更小）
    expect(shouldApplyProjection(snap, snapAt(3, 0, 1), CONTEXT)).toBe(true);
  });

  it("revision_none_rules：无 revision 的事件按 localSeq 竞争；不得证明比有 revision 的快照新", () => {
    const known = frameAt(12, 5);
    // (None, Some)：不能证明更新
    expect(shouldApplyProjection(known, frameAt(null, 99), CONTEXT)).toBe(false);
    // (Some, None)：有 revision 证明世界已更新
    expect(shouldApplyProjection(frameAt(null, 5), frameAt(12, 1), CONTEXT)).toBe(true);
    // (None, None)：本地重发布按序号决胜
    const emptyBase = frameAt(null, 5);
    expect(shouldApplyProjection(emptyBase, frameAt(null, 6), CONTEXT)).toBe(true);
    expect(shouldApplyProjection(emptyBase, frameAt(null, 5), CONTEXT)).toBe(false);
  });

  it("thread_mismatch_rejected：跨 thread 的投影不落地（服务端 supersedes 的 thread_id 检查同源）", () => {
    expect(shouldApplyProjection(newFrame, frameAt(99, 99, { threadId: "th-2" }), CONTEXT)).toBe(
      false
    );
  });
});

describe("codex-projection-order：instance 维度（跨服务重启）", () => {
  it("null_current_applies：无当前投影（首个 GET/SSE）直接落地", () => {
    expect(shouldApplyProjection(null, oldFrame, CONTEXT)).toBe(true);
  });

  it("instance_change_gated_by_allow_flag：instance 切换只由 allowInstanceChange 授权——旧 GET（false）无权切回旧 instance", () => {
    const currentNewWorld = frameAt(3, 3, {
      projectionVersion: ver({ instanceId: "inst-2", revision: 3, localSeq: 3 }),
    });
    const oldInstanceFrame = frameAt(99, 99, {
      projectionVersion: ver({ instanceId: "inst-1", revision: 99, localSeq: 99 }),
    });
    // 重启后旧请求（旧 instance、allowInstanceChange=false）不能装回——即使它
    // 的 revision/序号更高（跨世系不可比较，一票否决）。
    expect(shouldApplyProjection(currentNewWorld, oldInstanceFrame, CONTEXT)).toBe(false);
    // 当前代际最新握手（SSE snapshot / 首个成功 GET）：有权确立另一 instance
    //（真实世界 = 服务重启后的新快照；旧 GET 永远拿不到这个授权）。
    expect(
      shouldApplyProjection(currentNewWorld, oldInstanceFrame, {
        ...CONTEXT,
        allowInstanceChange: true,
      })
    ).toBe(true);
    // 反向（握手常规方向）：新 instance 覆盖旧世界
    const currentOldWorld = frameAt(11, 11, {
      projectionVersion: ver({ instanceId: "inst-1", revision: 11, localSeq: 11 }),
    });
    const newWorldFrame = frameAt(12, 12, { projectionVersion: ver({ instanceId: "inst-3" }) });
    expect(
      shouldApplyProjection(currentOldWorld, newWorldFrame, {
        ...CONTEXT,
        allowInstanceChange: true,
      })
    ).toBe(true);
    // 无授权（非握手的普通帧/旧 GET）：不得切换 instance
    expect(shouldApplyProjection(currentOldWorld, newWorldFrame, CONTEXT)).toBe(false);
  });
});

describe("codex-projection-order：订阅代际（在途响应作废）", () => {
  it("older_subscription_generation_rejected：切订阅代际后的在途响应一律作废（含 current=null）", () => {
    const lateCtx: ProjectionApplyContext = {
      ...CONTEXT,
      subscriptionGeneration: 4,
      responseSubscriptionGeneration: 3,
    };
    expect(shouldApplyProjection(null, newFrame, lateCtx)).toBe(false);
    expect(shouldApplyProjection(oldFrame, newFrame, lateCtx)).toBe(false);
  });
});

describe("codex-projection-order：stale 顺序（断链禁写，不用 fresh 永远赢）", () => {
  it("newer_stale_must_land：较新的断链必须落地（禁写），同 revision 的 stale 也送达", () => {
    const fresh13 = frameAt(13, 13);
    // 较新断链（更高 revision）
    expect(shouldApplyProjection(fresh13, frameAt(14, 14, { stale: true }), CONTEXT)).toBe(true);
    // 同 revision 的 stale：保守送达（宁禁写、不假可控）
    expect(shouldApplyProjection(fresh13, frameAt(13, 14, { stale: true }), CONTEXT)).toBe(true);
  });

  it("older_stale_never_overrides_recovery：较旧 stale 不覆盖新恢复状态", () => {
    // 恢复已到 rev 14，迟到的 rev 13 断链不得把卡面拉回禁写
    expect(shouldApplyProjection(frameAt(14, 14), frameAt(13, 15, { stale: true }), CONTEXT)).toBe(
      false
    );
  });

  it("recovery_needs_higher_revision：恢复需更高 revision（同 revision 的非 stale 不证明新数据）", () => {
    const stale13 = frameAt(13, 13, { stale: true });
    expect(shouldApplyProjection(stale13, frameAt(13, 99), CONTEXT)).toBe(false);
    expect(shouldApplyProjection(stale13, frameAt(14, 1), CONTEXT)).toBe(true);
    // 当前无 revision 的 stale：任何带 revision 的恢复都证明更新
    const staleNoRev = frameAt(null, 3, { stale: true });
    expect(shouldApplyProjection(staleNoRev, frameAt(1, 1), CONTEXT)).toBe(true);
  });

  it("stale_chain_advances_by_revision：断链态内较新的 stale 继续落地（真实断链不被吞）", () => {
    expect(
      shouldApplyProjection(
        frameAt(13, 13, { stale: true }),
        frameAt(14, 14, { stale: true }),
        CONTEXT
      )
    ).toBe(true);
  });
});

describe("codex-projection-order：旧载荷兼容（缺 projectionVersion 退化）", () => {
  it("legacy_falls_back_to_revision_rules：缺版本束退化为 revision 规则（同 revision 落地；更低拒绝）", () => {
    const noVersion12 = legacy(frameAt(12, 12));
    // 同 revision：旧世界语义 = 落地（开关/暂停 republish 帧、重发快照无副作用）
    expect(shouldApplyProjection(newFrame, noVersion12, CONTEXT)).toBe(true);
    // 更低 revision：不是新世界
    expect(shouldApplyProjection(newFrame, legacy(frameAt(11, 11)), CONTEXT)).toBe(false);
    // 更高 revision：新世界
    expect(shouldApplyProjection(newFrame, legacy(frameAt(13, 13)), CONTEXT)).toBe(true);
    // 双方都无版本束：同语义
    expect(shouldApplyProjection(noVersion12, legacy(frameAt(11, 11)), CONTEXT)).toBe(false);
    expect(shouldApplyProjection(noVersion12, legacy(frameAt(13, 13)), CONTEXT)).toBe(true);
    // incoming 有束、current 无束（混载过渡期）：按 revision 比较仍可落地
    expect(shouldApplyProjection(legacy(frameAt(12, 12)), newFrame, CONTEXT)).toBe(true);
  });
});

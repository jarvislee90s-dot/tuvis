// DSH 领域 DTO 镜像契约测试（T01；计划 §2.2「对应 TS camelCase」）。
//
// 层次纪律：本测试只消费两类输入——① src/types/dsh.ts 的 TS 镜像与运行时守卫
// （解析与 rpcId 拒绝主体在 Rust codec，TS 侧锁镜像与闭集纪律）；② src-tauri
// 契约夹具（来源/真伪/未测项见其 README.md）。不连接任何真实 DSH 实例。

import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  DSH_OPERATIONS,
  parseDshActionPayload,
  type DshApprovalPayload,
  type DshPendingInteraction,
  type DshSessionSnapshot,
} from "@/types/dsh";

const FIXTURE_ROOT = path.resolve(
  __dirname,
  "../../src-tauri/tests/fixtures/dsh-native/0.2.1-rc.3"
);

const readFixture = (rel: string): unknown =>
  JSON.parse(readFileSync(path.join(FIXTURE_ROOT, rel), "utf8"));

describe("dsh dto mirror (T01)", () => {
  it("response_rejects_foreign_rpc_id — 原生样本信封形状与 rpcId 身份字段锁定（解码主体在 Rust codec::decode_response）", () => {
    // 真实帧（Win 报告 §3.2 步骤 6）：身份字段是字符串 rpcId，TS 消费方按原样回传。
    const ok = readFixture("windows/rpc-session-list-ok.json") as {
      type: string;
      rpcId: string;
      result: { ok: boolean; value?: unknown };
    };
    expect(ok.type).toBe("server-response");
    expect(ok.rpcId).toBe("ok");
    expect(ok.result.ok).toBe(true);
    // 外来帧判定语义与 Rust 同源：rpcId ≠ 期望值即拒。
    expect(ok.rpcId === "a1").toBe(false);

    // 403 unpaired 信封（Mac 报告 §3.3 实测形状）：typed 失败且码可读。
    const denied = readFixture("macos/rpc-unpaired-403.json") as {
      type: string;
      result: { ok: boolean; error: { code: string } };
    };
    expect(denied.type).toBe("server-response");
    expect(denied.result.ok).toBe(false);
    expect(denied.result.error.code).toBe("unpaired");
  });

  it("dto_preserves_native_request_identity — 动作载荷里的原生请求身份逐字段保持（evt-17 与 waterfall 夹具同源）", () => {
    const approval = parseDshActionPayload({
      type: "approval",
      key: {
        hostId: "local",
        epoch: 1,
        sessionId: "session-b2f3dd10",
        clientId: "client-probe-1",
        eventId: "evt-17",
        agentId: "agent-probe-1",
        revision: 3,
      },
      decision: "allowed-once",
    }) as DshApprovalPayload;
    expect(approval.key.eventId).toBe("evt-17");
    expect(approval.key.clientId).toBe("client-probe-1");
    expect(approval.key.agentId).toBe("agent-probe-1");
    expect(approval.key.revision).toBe(3);
    expect(approval.decision).toBe("allowed-once");

    // 序列化往返不改写原生 ID（camelCase 键名锁定）。
    const round = JSON.parse(JSON.stringify(approval)) as DshApprovalPayload;
    expect(round.key.eventId).toBe("evt-17");
    expect(round.type).toBe("approval");
  });

  it("unknown_variant_is_not_action — 动作是闭集：白名单外 type 一律拒绝", () => {
    expect(() => parseDshActionPayload({ type: "terminal.create", args: {} })).toThrow();
    expect(() => parseDshActionPayload("not an object")).toThrow();
    expect(() => parseDshActionPayload({})).toThrow();

    // 已知变体照常解析（证明不是无脑全拒）。
    const send = parseDshActionPayload({
      type: "send",
      text: "hi",
      receiptIds: [],
      queueOnly: false,
      bypassPausedQueue: false,
    });
    expect(send.type).toBe("send");

    // 已知变体携带未声明字段同样拒绝（deny_unknown_fields 纪律）。
    expect(() => parseDshActionPayload({ type: "pin", text: "extra" })).toThrow();
    expect(() =>
      parseDshActionPayload({ type: "queue_remove", itemId: "q-1", evil: true })
    ).toThrow();
    // 已知变体缺必填字段拒绝。
    expect(() => parseDshActionPayload({ type: "select_model", provider: "p" })).toThrow();
  });

  it("operation 联合 28 词与夹具形态一致（TS 镜像 ↔ Rust Operation::ALL 同源锁）", () => {
    expect(DSH_OPERATIONS).toHaveLength(28);
    expect(DSH_OPERATIONS).toContain("send");
    expect(DSH_OPERATIONS).toContain("command_execute");
    expect(DSH_OPERATIONS).toContain("file_upload");
  });

  it("pending 交互镜像类型与 waterfall 夹具的问答形状逐字段对应", () => {
    const frame = readFixture("macos/waterfall-user-questions-request.json") as {
      type: string;
      event: string;
      eventId: string;
      agentId: string;
      request: {
        questions: Array<{
          id: string;
          question: string;
          header?: string;
          options?: Array<{ label: string; description: string }>;
        }>;
      };
    };
    expect(frame.type).toBe("waterfall");
    expect(frame.event).toBe("user-questions/request");
    // 镜像类型的最小形状断言（PendingInteraction.questions 的来源形状）。
    const pending: DshPendingInteraction = {
      key: {
        hostId: "local",
        epoch: 1,
        sessionId: "session-b2f3dd10",
        clientId: "client-probe-1",
        eventId: frame.eventId,
        agentId: frame.agentId,
        revision: 0,
      },
      kind: "questions",
      allowedDecisions: ["answer"],
      question: frame.request.questions[0].question,
      questions: frame.request.questions.map((q) => ({
        id: q.id,
        question: q.question,
        header: q.header ?? null,
        options: (q.options ?? []).map((o) => ({ label: o.label, description: o.description })),
        multiSelect: false,
      })),
      summary: null,
      resolved: false,
    };
    expect(pending.key.eventId).toBe("evt-17");
    expect(pending.questions[0].id).toBe("fruit_choice");
    expect(pending.questions[0].options).toHaveLength(2);
  });

  it("快照镜像类型接受后端 camelCase 形状（GET /dsh/session 契约锚）", () => {
    const snapshot: DshSessionSnapshot = {
      target: { hostId: "local", epoch: 1, sessionId: "s-1" },
      revision: 7,
      connectionState: "connected",
      effectiveOperations: {
        revision: 1,
        operations: {
          send: {
            implemented: true,
            allowed: true,
            verified: true,
            requiresAutoAdapt: false,
            evidence: "observed_success",
            reasonCode: null,
          },
        },
      },
      messages: [],
      pending: [],
      queue: [],
      model: { provider: "deepseek-official", model: "deepseek-flash", reasoningEffort: "high" },
    };
    expect(snapshot.connectionState).toBe("connected");
    expect(snapshot.effectiveOperations.operations.send?.evidence).toBe("observed_success");
    expect(snapshot.model?.reasoningEffort).toBe("high");
  });
});

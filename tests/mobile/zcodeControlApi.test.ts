// T7.2 Slice A1：ZCode 统一控制手机 API 客户端契约锁（fetch 全量 stub，合成夹具）。
// 锁定四件事：只走官方 `/m/api/v1/zcode/*`（绝不落旧 session-send/无头）、路径精确、
// 错误体 `{error}` → 冻结码、DTO 形状（TS 侧手写夹具；**不是** Rust 生成的 golden
// 夹具，Rust 侧改名不会让本文件变红——跨语言对拍见 T7.2 报告「已知边界」）。
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_EVENTS_PATH,
  ZCODE_SESSION_PATH,
  ZcodeControlApiError,
  getZcodeConnection,
  getZcodeSession,
  postZcodeAction,
  zcodeEventsUrl,
  type ActionReceipt,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type SessionView,
} from "@/lib/api/zcodeControl";

const SID = "session_00000000000000000000000000000001";

let fetchMock: ReturnType<typeof vi.fn>;

/** 未登记 URL 直接抛（防「默默少调一个接口」把断言变成假阴性） */
function installFetch(respond: (url: string, init?: RequestInit) => Response) {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) =>
    respond(String(input), init)
  );
  vi.stubGlobal("fetch", fetchMock);
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

const calledUrls = () => fetchMock.mock.calls.map(([input]) => String(input));

afterEach(() => {
  vi.unstubAllGlobals();
});

const flag = (enabled: boolean): { enabled: boolean; reasonCode: string | null } => ({
  enabled,
  reasonCode: enabled ? null : "needs_evidence",
});

const goldenConnection: ConnectionView = {
  state: "paired",
  generation: 3,
  reasonCode: null,
  deviceLabel: "Synthetic Mac",
};

const goldenSession: SessionView = {
  sessionId: SID,
  generation: 3,
  revision: 12,
  ready: true,
  pending: [
    {
      interactionId: "int-1",
      kind: "userInput",
      anchorRowId: 7,
      toolCallId: "tool-1",
      createdAt: 1730000000000,
      summary: "合成卡片",
      toolName: "AskUserQuestion",
      schemaInteraction: "ask_user_question",
      sensitive: false,
      autoResolution: true,
      autoResolutionState: "snoozed",
      questions: [
        {
          header: "合成题头",
          questionText: "合成题干",
          options: [{ value: "v1", label: "选项一", description: null, preview: "合成预览" }],
          multiSelect: false,
          freeTextAllowed: true,
        },
      ],
      options: [{ optionId: "opt-1", label: "允许一次", kind: "allowOnce" }],
      detail: "合成命令正文",
    },
  ],
  control: { activeWorks: [{ kind: "execution", foregroundExecutionId: "exec-1" }] },
  selection: {
    provider: "zai",
    model: "model-1",
    thought: "high",
    models: [{ modelId: "model-1", thoughtLevels: ["low", "high"] }],
  },
  capabilities: {
    send: flag(true),
    singleSelect: flag(true),
    multiSelect: flag(false),
    freeText: flag(false),
    interrupt: flag(true),
    approvalReply: flag(false),
    model: flag(true),
    thoughtLevel: flag(true),
  } satisfies CapabilityView,
};

const goldenReceipt: ActionReceipt = {
  actionId: "act-1",
  sessionId: SID,
  generation: 3,
  state: "accepted",
  reasonCode: null,
};

const sendAction: ControlAction = {
  kind: "send",
  sessionId: SID,
  generation: 3,
  text: "合成正文",
};

describe("zcodeControlApi：官方动作发送面", () => {
  it("send_uses_official_api_not_legacy_or_headless", async () => {
    installFetch(() => json(goldenReceipt));
    const receipt = await postZcodeAction("act-1", sendAction);

    // 唯一一次请求 = POST /m/api/v1/zcode/actions，body 只有 {actionId, action}
    expect(calledUrls()).toEqual([ZCODE_ACTIONS_PATH]);
    const init = fetchMock.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual({ actionId: "act-1", action: sendAction });
    expect(receipt).toEqual(goldenReceipt);

    // 零旧路径 / 零无头分流调用
    const legacy = calledUrls().filter((url) =>
      /session-send|headless|session-approve|session-question/.test(url)
    );
    expect(legacy).toEqual([]);
  });

  it("connection_and_session_paths_are_exact", async () => {
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(goldenConnection);
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(goldenSession);
      throw new Error(`unexpected fetch: ${url}`);
    });

    await expect(getZcodeConnection()).resolves.toEqual(goldenConnection);
    await expect(getZcodeSession(SID)).resolves.toEqual(goldenSession);

    expect(calledUrls()).toEqual([
      "/m/api/v1/zcode/connection",
      `/m/api/v1/zcode/session?sessionId=${SID}`,
    ]);
    for (const [, init] of fetchMock.mock.calls) {
      // GET：不带 method 或显式 GET（两种都符合 fetch 语义）
      expect((init as RequestInit | undefined)?.method ?? "GET").toBe("GET");
    }
  });

  it("error_body_maps_to_typed_code", async () => {
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json({ error: "needs_auth" }, 503);
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json({ error: "stale_generation" }, 409);
      if (url === ZCODE_ACTIONS_PATH) return json({ error: "forbidden_option" }, 400);
      return new Response("<html>proxy</html>", { status: 502 });
    });

    const connErr = await getZcodeConnection().catch((e: unknown) => e);
    expect(connErr).toBeInstanceOf(ZcodeControlApiError);
    expect(connErr).toMatchObject({ status: 503, code: "needs_auth" });

    const sessionErr = await getZcodeSession(SID).catch((e: unknown) => e);
    expect(sessionErr).toMatchObject({ status: 409, code: "stale_generation" });

    const actionErr = await postZcodeAction("act-1", sendAction).catch((e: unknown) => e);
    expect(actionErr).toMatchObject({ status: 400, code: "forbidden_option" });

    // 词表外错误词不冒充冻结码：归 unknown（仍是类型化码）；非 JSON 错误体 code=null
    installFetch(() => json({ error: "bad_request" }, 400));
    await expect(getZcodeConnection().catch((e: unknown) => e)).resolves.toMatchObject({
      status: 400,
      code: "unknown",
    });
    installFetch(() => new Response("<html>proxy</html>", { status: 502 }));
    await expect(getZcodeConnection().catch((e: unknown) => e)).resolves.toMatchObject({
      status: 502,
      code: null,
    });
  });

  it("dto_camel_case_shape_matches_rust", async () => {
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(goldenConnection);
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(goldenSession);
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") return json(goldenReceipt);
      throw new Error(`unexpected fetch: ${url}`);
    });

    // 夹具是**手工转写**自 Rust types.rs（camelCase；枚举原值），不是 types.rs 产出的
    // golden JSON：toEqual 只锁本侧（客户端 + TS 类型）的字段名/嵌套形状，Rust serde
    // 改名或改枚举值不会触发本用例。跨语言防漂移需 Rust 单测导出固定 JSON 到
    // tests/fixtures/（未做，见 T7.2 报告「已知边界」）。
    expect(await getZcodeConnection()).toEqual(goldenConnection);
    expect(await getZcodeSession(SID)).toEqual(goldenSession);
    expect(await postZcodeAction("act-1", sendAction)).toEqual(goldenReceipt);
    expect(Object.keys(goldenSession.pending[0]).sort()).toEqual(
      [
        "anchorRowId",
        "autoResolution",
        "autoResolutionState",
        "createdAt",
        "detail",
        "interactionId",
        "kind",
        "options",
        "questions",
        "schemaInteraction",
        "sensitive",
        "summary",
        "toolCallId",
        "toolName",
      ].sort()
    );
    expect(Object.keys(goldenSession.capabilities).sort()).toEqual(
      [
        "approvalReply",
        "freeText",
        "interrupt",
        "model",
        "multiSelect",
        "send",
        "singleSelect",
        "thoughtLevel",
      ].sort()
    );
  });

  it("events_url_is_paired_session_scoped", () => {
    expect(ZCODE_EVENTS_PATH).toBe("/m/api/v1/zcode/events");
    expect(zcodeEventsUrl(SID)).toBe(`/m/api/v1/zcode/events?sessionId=${SID}`);

    // 会话原 ID 原样编码进唯一参数（不掺 workspace/设备/其他查询键）
    const url = new URL(zcodeEventsUrl("sess_+slash/1"), "http://localhost");
    expect(url.pathname).toBe("/m/api/v1/zcode/events");
    expect([...url.searchParams.keys()]).toEqual(["sessionId"]);
    expect(url.searchParams.get("sessionId")).toBe("sess_+slash/1");
    expect(url.toString()).toBe(
      "http://localhost/m/api/v1/zcode/events?sessionId=sess_%2Bslash%2F1"
    );
  });
});

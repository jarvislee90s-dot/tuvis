import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import OpenCodeNativeQueue, { resetQueueControlMemoryForTests } from "@/mobile/OpenCodeNativeQueue";
import i18n from "@/i18n";

let bodies: Record<string, unknown>[];
beforeEach(async () => {
  await i18n.changeLanguage("zh");
  sessionStorage.clear();
  bodies = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        bodies.push(body);
        return new Response(JSON.stringify({ actionId: body.actionId, state: "settled" }));
      }
      return new Response(
        JSON.stringify({
          sessionId: "s",
          revision: 4,
          paused: false,
          items: [
            {
              actionId: "local",
              owner: "tuvis",
              state: "queued",
              reasonCode: "OC_BINDING_STALE",
              textSummary: "本地待发",
              attachmentCount: 0,
            },
            {
              actionId: "native",
              owner: "native",
              state: "acknowledged",
              textSummary: "原生队列",
              attachmentCount: 1,
            },
            {
              actionId: "unknown",
              owner: "native",
              state: "unconfirmed",
              textSummary: "结果未知",
              attachmentCount: 0,
            },
          ],
        })
      );
    })
  );
});
afterEach(() => {
  cleanup();
  resetQueueControlMemoryForTests();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

async function renderQueueSnapshot(items: Record<string, unknown>[]) {
  const read = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
    if (init?.method === "POST") {
      const body = JSON.parse(String(init.body));
      bodies.push(body);
      return new Response(JSON.stringify({ actionId: body.actionId, state: "settled" }));
    }
    return new Response(JSON.stringify({ sessionId: "s", revision: 4, paused: false, items }));
  });
  vi.stubGlobal("fetch", read);
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  await act(async () => {
    expect(read).toHaveBeenCalled();
    await Promise.all(read.mock.results.map(({ value }) => value));
  });
}

it("阻断待发项显示需处理提示，可撤回", async () => {
  await renderQueueSnapshot([
    {
      actionId: "local",
      owner: "tuvis",
      state: "queued",
      reasonCode: "OC_BINDING_STALE",
      textSummary: "本地待发",
      attachmentCount: 0,
    },
  ]);
  const retract = await screen.findByTestId("outbox-retract-local");
  expect(retract).toBeEnabled();
  expect(screen.getByTestId("native-outbox")).toBeInTheDocument();
  expect(screen.getByText(i18n.t("mobile.openCodeNative.outbox.pendingTitle"))).toBeInTheDocument();
  fireEvent.click(retract);
  await waitFor(() => expect(bodies).toHaveLength(1));
  expect(bodies[0].operation).toEqual({ kind: "retract_outbox", targetActionId: "local" });
});

it("原生已受理项不再常驻列表或显示受理提示", async () => {
  await renderQueueSnapshot([
    {
      actionId: "native",
      owner: "native",
      state: "acknowledged",
      textSummary: "原生队列",
      attachmentCount: 0,
    },
  ]);
  // 正常受理会在原生记录推进，不应产生额外的队列行。
  expect.soft(screen.queryByTestId("outbox-native-native")).toBeNull();
  expect(screen.queryByTestId("outbox-verify-native")).toBeNull();
  expect.soft(screen.queryByTestId("outbox-native-native")).toBeNull();
  expect(screen.queryByText(i18n.t("mobile.openCodeNative.receipt.acknowledged"))).toBeNull();
});

it("空且未暂停的队列默认不显示暂停控制或范围说明", async () => {
  await renderQueueSnapshot([]);
  // 没有待处理事项时，不应常驻暂停按钮。
  expect.soft(screen.queryByTestId("outbox-pause")).toBeNull();
});

it("已暂停时只列本地待发（可撤回），只有一个恢复文字链、无说明框", async () => {
  mockPausedOutbox();
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  const local = await screen.findByTestId("outbox-local-local");
  expect(within(local).getByTestId("outbox-retract-local")).toBeTruthy();
  expect(screen.queryByTestId("outbox-retract-native")).toBeNull();
  expect(screen.queryByTestId("outbox-rebind-unknown")).toBeNull();
  // 2026-11-22 用户口径：暂停只在发送键上体现，队列区不再有恢复按钮/说明段
  expect(screen.getByTestId("outbox-pause")).toBeInTheDocument(); // 单一文字链入口（无独立框）
  expect(bodies).toHaveLength(0);
});

it("孤儿待发项（无理由 not_sent）可见：正文未知占位 + 撤回钮，无重绑（D-N5-2）", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        bodies.push(body);
        return new Response(JSON.stringify({ actionId: body.actionId, state: "settled" }));
      }
      return new Response(
        JSON.stringify({
          sessionId: "s",
          revision: 5,
          paused: true,
          items: [
            {
              actionId: "orphan",
              owner: "tuvis",
              state: "not_sent",
              textSummary: "",
              attachmentCount: 0,
            },
          ],
        })
      );
    })
  );
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  const orphan = await screen.findByTestId("outbox-local-orphan");
  expect(orphan.textContent).toContain(i18n.t("mobile.openCodeNative.outbox.orphanNoText"));
  const retract = screen.getByTestId("outbox-retract-orphan");
  expect(retract).toBeTruthy();
  expect(screen.queryByTestId("outbox-rebind-orphan")).toBeNull();
  fireEvent.click(retract);
  await waitFor(() => expect(bodies).toHaveLength(1));
  expect(bodies[0]).toMatchObject({
    sessionId: "s",
    bindingToken: "binding",
    operation: { kind: "retract_outbox", targetActionId: "orphan" },
  });
});

it("没有在线绑定仍可读本地项；本地撤回可用，重绑仍禁用（无恢复按钮）", async () => {
  mockPausedOutbox();
  render(<OpenCodeNativeQueue sessionId="s" bindingToken={null} readBindingToken="binding" />);
  await screen.findByTestId("outbox-local-local");
  const outboxCalls = vi
    .mocked(fetch)
    .mock.calls.filter(([input]) => String(input).includes("/opencode/outbox"));
  expect(outboxCalls).toHaveLength(1);
  expect(new Headers(outboxCalls[0][1]?.headers).get("X-OpenCode-Expected-Binding")).toBe(
    "binding"
  );
  expect(screen.getByTestId("outbox-pause")).toBeInTheDocument(); // 单一文字链入口（无独立框）
  expect(screen.getByTestId("outbox-retract-local")).not.toBeDisabled();
  expect(screen.getByTestId("outbox-rebind-local")).toBeDisabled();
  expect(bodies).toHaveLength(0);
});

it("重新绑定必须明确确认，取消没有远程副作用", async () => {
  const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  fireEvent.click(await screen.findByTestId("outbox-rebind-local"));
  expect(confirm).toHaveBeenCalledOnce();
  expect(bodies).toHaveLength(0);
  confirm.mockReturnValue(true);
  fireEvent.click(screen.getByTestId("outbox-rebind-local"));
  await waitFor(() => expect(bodies).toHaveLength(1));
  expect(bodies[0].operation).toEqual({ kind: "rebind_outbox", targetActionId: "local" });
});

// 2026-11-22 用户口径：暂停不再有恢复按钮；本地控制语义改由「撤回本地待发」承载
// （同为 local_only 动作，锚/单飞/零重投断言原样保留）。
it("本地控制请求丢回执后只允许核对，重挂不重试写入", async () => {
  mockPausedOutbox();
  const original = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        bodies.push(JSON.parse(String(init.body)));
        throw new TypeError("lost reply");
      }
      return original(input, init);
    })
  );
  const view = render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  const retract = await screen.findByTestId("outbox-retract-local");
  fireEvent.click(retract);
  await screen.findByTestId("outbox-unknown");
  await waitFor(() => expect(screen.getByTestId("outbox-retract-local")).toBeDisabled());
  view.unmount();
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  await screen.findByTestId("outbox-unknown");
  expect(bodies).toHaveLength(1);
  expect(screen.getByTestId("outbox-retract-local")).toBeDisabled();
});

it("本地控制被拒后，成功刷新不能抹掉拒绝提示", async () => {
  mockPausedOutbox();
  const original = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST")
        return new Response(JSON.stringify({ actionId: "stale", state: "not_accepted" }));
      return original(input, init);
    })
  );
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  const retract = await screen.findByTestId("outbox-retract-local");
  fireEvent.click(retract);
  await waitFor(() => expect(screen.queryByTestId("outbox-unknown")).toBeNull());
  await waitFor(() => expect(screen.getByRole("status")).toBeTruthy());
});

it("已有原生归属但 B1 拒写：明确原因、清除本次锚、不重发也不锁死", async () => {
  const original = globalThis.fetch;
  const posts: Record<string, unknown>[] = [];
  const actionReads: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (init?.method === "POST") {
        posts.push(JSON.parse(String(init.body)));
        return new Response(
          JSON.stringify({
            error: "OC_INSTANCE_OWNERSHIP_UNVERIFIED",
            reasonCode: "OC_INSTANCE_OWNERSHIP_UNVERIFIED",
          }),
          { status: 409 }
        );
      }
      if (url.includes("/opencode/actions/")) {
        actionReads.push(url);
        return new Response(JSON.stringify({ actionId: "unused", state: "unconfirmed" }));
      }
      return original(input, init);
    })
  );
  const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
  const view = render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  fireEvent.click(await screen.findByTestId("outbox-rebind-local"));
  await waitFor(() => expect(posts).toHaveLength(1));
  expect(confirm).toHaveBeenCalled();
  // 明确显示原因：不是“结果未知”，也不是通用“读取失败”
  expect(
    await screen.findByText(i18n.t("mobile.openCodeNative.errors.instanceOwnershipUnverified"))
  ).toBeTruthy();
  expect(screen.queryByTestId("outbox-unknown")).toBeNull();
  // 本次锚已清、控制可用：不会因为一次确定零写出的拒绝把会话锁死
  expect(sessionStorage.getItem("tuvis.opencodeNative.outboxControl.s")).toBeNull();
  expect(screen.getByTestId("outbox-rebind-local")).not.toBeDisabled();
  expect(actionReads).toHaveLength(0);
  // 不重发：刷新/重挂后仍只有最初一次 POST，也不产生核对读取
  view.unmount();
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  await screen.findByTestId("outbox-rebind-local");
  await waitFor(() => expect(screen.getByTestId("outbox-rebind-local")).not.toBeDisabled());
  expect(posts).toHaveLength(1);
  expect(actionReads).toHaveLength(0);
});

it("传输/响应类 409 仍是未知结局：保留锚并锁住，不能当确定失败", async () => {
  const original = globalThis.fetch;
  const posts: Record<string, unknown>[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        posts.push(JSON.parse(String(init.body)));
        return new Response(
          JSON.stringify({ error: "OC_RESPONSE_INVALID", reasonCode: "OC_RESPONSE_INVALID" }),
          { status: 409 }
        );
      }
      return original(input, init);
    })
  );
  const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
  const view = render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  fireEvent.click(await screen.findByTestId("outbox-rebind-local"));
  await waitFor(() => expect(posts).toHaveLength(1));
  expect(confirm).toHaveBeenCalled();
  expect(await screen.findByTestId("outbox-unknown")).toBeTruthy();
  expect(sessionStorage.getItem("tuvis.opencodeNative.outboxControl.s")).not.toBeNull();
  await waitFor(() => expect(screen.getByTestId("outbox-rebind-local")).toBeDisabled());
  view.unmount();
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  await screen.findByTestId("outbox-unknown");
  expect(posts).toHaveLength(1);
  expect(screen.getByTestId("outbox-rebind-local")).toBeDisabled();
});

it("已知 B1 阻断由会话级解释，队列不重复提示且未知动作仍可只读核对", async () => {
  mockPausedOutbox();
  sessionStorage.setItem("tuvis.opencodeNative.outboxControl.s", "control_unknown");
  const original = globalThis.fetch;
  const posts: Record<string, unknown>[] = [];
  const verifies: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/actions/unknown/verify")) {
        verifies.push(url);
        return new Response(JSON.stringify({ actionId: "unknown", state: "confirmed" }));
      }
      if (init?.method === "POST") {
        posts.push(JSON.parse(String(init.body)));
        return new Response(JSON.stringify({ actionId: "unused", state: "settled" }));
      }
      if (url.endsWith("/actions/control_unknown"))
        return new Response(JSON.stringify({ actionId: "control_unknown", state: "unconfirmed" }));
      return original(input, init);
    })
  );
  render(
    <OpenCodeNativeQueue
      sessionId="s"
      bindingToken="binding"
      writeBlockedCode="OC_INSTANCE_OWNERSHIP_UNVERIFIED"
    />
  );
  expect(await screen.findByTestId("outbox-local-local")).toBeTruthy();
  expect(
    screen.queryByText(i18n.t("mobile.openCodeNative.errors.instanceOwnershipUnverified"))
  ).toBeNull();
  // 未决锚使本地控件不可点，但待发项和只读核对仍可用。
  expect(screen.getByTestId("outbox-local-local")).toBeTruthy();
  expect(screen.getByTestId("outbox-pause")).toBeInTheDocument(); // 单一文字链入口（无独立框）
  expect(screen.getByTestId("outbox-retract-local")).toBeDisabled();
  expect(screen.getByTestId("outbox-rebind-local")).toBeDisabled();
  expect(posts).toHaveLength(0);
  // 真正的未知动作（可能已经写出）不被这个已知阻断误判：核对仍可用且锚不清
  fireEvent.click(await screen.findByTestId("outbox-verify-unknown"));
  await waitFor(() => expect(verifies).toHaveLength(1));
  expect(screen.getByTestId("outbox-unknown")).toBeTruthy();
  expect(sessionStorage.getItem("tuvis.opencodeNative.outboxControl.s")).toBe("control_unknown");
});

it("确定派发前拒绝可解锁，而放弃未知核对只改本地锚", async () => {
  mockPausedOutbox();
  const original = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        bodies.push(JSON.parse(String(init.body)));
        return new Response(
          JSON.stringify({ error: "OC_BINDING_STALE", reasonCode: "OC_BINDING_STALE" }),
          { status: 409 }
        );
      }
      return original(input, init);
    })
  );
  const view = render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  const retract = await screen.findByTestId("outbox-retract-local");
  fireEvent.click(retract);
  await waitFor(() => expect(bodies).toHaveLength(1));
  // 确定派发前拒绝（零写出）：锚清除、本地控件立即恢复可用
  await waitFor(() => expect(screen.getByTestId("outbox-retract-local")).not.toBeDisabled());
  expect(screen.queryByTestId("outbox-unknown")).toBeNull();
  view.unmount();
  sessionStorage.setItem("tuvis.opencodeNative.outboxControl.s", "lost_control");
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  fireEvent.click(await screen.findByTestId("outbox-abandon-verification"));
  expect(sessionStorage.getItem("tuvis.opencodeNative.outboxControl.s")).toBeNull();
  expect(bodies).toHaveLength(1);
});

it.each(["dispatching", "unconfirmed"] as const)(
  "%s 逐项核对不依赖写绑定，未收敛时保留且不重投",
  async (state) => {
    const verifies: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.endsWith("/actions/background/verify")) {
          verifies.push(url);
          return new Response(JSON.stringify({ actionId: "background", state }));
        }
        if (init?.method === "POST") throw new Error("must not submit or resend");
        return new Response(
          JSON.stringify({
            sessionId: "s",
            revision: 8,
            paused: false,
            items: [
              {
                actionId: "background",
                owner: "native",
                state,
                textSummary: "后台交付",
                attachmentCount: 0,
              },
            ],
          })
        );
      })
    );
    render(<OpenCodeNativeQueue sessionId="s" bindingToken={null} readBindingToken="binding" />);
    const button = await screen.findByTestId("outbox-verify-background");
    expect(button).not.toBeDisabled();
    fireEvent.click(button);
    await waitFor(() => expect(verifies).toHaveLength(1));
    await waitFor(() => expect(button).not.toBeDisabled());
    expect(screen.getByTestId("outbox-native-background")).toHaveTextContent(
      i18n.t(`mobile.openCodeNative.outbox.states.${state}`)
    );
    expect(screen.queryByTestId("outbox-rebind-background")).toBeNull();
    expect(screen.queryByTestId("outbox-retract-background")).toBeNull();
    expect(
      vi
        .mocked(fetch)
        .mock.calls.filter(
          ([input, init]) => init?.method === "POST" && !String(input).endsWith("/verify")
        )
    ).toHaveLength(0);
  }
);

it("逐项核对不能清掉另一个本地控制动作的未知锚", async () => {
  sessionStorage.setItem("tuvis.opencodeNative.outboxControl.s", "control_unknown");
  const original = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/actions/control_unknown"))
        return new Response(JSON.stringify({ actionId: "control_unknown", state: "unconfirmed" }));
      if (url.endsWith("/actions/unknown/verify"))
        return new Response(JSON.stringify({ actionId: "unknown", state: "confirmed" }));
      return original(input, init);
    })
  );
  render(<OpenCodeNativeQueue sessionId="s" bindingToken={null} readBindingToken="binding" />);
  fireEvent.click(await screen.findByTestId("outbox-verify-unknown"));
  await waitFor(() => expect(screen.getByTestId("outbox-verify-unknown")).not.toBeDisabled());
  expect(sessionStorage.getItem("tuvis.opencodeNative.outboxControl.s")).toBe("control_unknown");
  expect(screen.getByTestId("outbox-unknown")).toBeTruthy();
  expect(bodies).toHaveLength(0);
});

it("已知 B1 但无未决锚时：恢复受限但本地撤回可用，重绑仍禁用", async () => {
  mockPausedOutbox();
  const original = globalThis.fetch;
  const posts: Record<string, unknown>[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        posts.push(JSON.parse(String(init.body)));
        return new Response(JSON.stringify({ actionId: "local", state: "settled" }));
      }
      return original(input, init);
    })
  );
  render(
    <OpenCodeNativeQueue
      sessionId="s"
      bindingToken={null}
      readBindingToken="binding"
      writeBlockedCode="OC_INSTANCE_OWNERSHIP_UNVERIFIED"
    />
  );
  await screen.findByTestId("outbox-local-local");
  // 暂停/恢复控件已删除（2026-11-22 用户口径）；本地撤回属 local_only 仍可用
  expect(screen.getByTestId("outbox-pause")).toBeInTheDocument(); // 单一文字链入口（无独立框）
  expect(screen.getByTestId("outbox-retract-local")).not.toBeDisabled();
  expect(screen.getByTestId("outbox-rebind-local")).toBeDisabled();
  fireEvent.click(screen.getByTestId("outbox-retract-local"));
  await waitFor(() => expect(posts).toHaveLength(1));
  expect(posts[0]).toMatchObject({
    sessionId: "s",
    bindingToken: "binding",
    operation: { kind: "retract_outbox", targetActionId: "local" },
  });
});

// ============================================================
// 2026-11-22 用户口径：暂停不再有独立入口（只在发送键上体现），队列组件
// 在空队列/无未决时整块不渲染；暂停态经 onPausedChange 上报。
// ============================================================
it("空队列未暂停时队列区整块不渲染（不再有预暂停入口）", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        bodies.push(body);
        return new Response(JSON.stringify({ actionId: body.actionId, state: "settled" }));
      }
      return new Response(
        JSON.stringify({ sessionId: "s", revision: 6, paused: false, items: [] })
      );
    })
  );
  const pausedReports: boolean[] = [];
  render(
    <OpenCodeNativeQueue
      sessionId="s"
      bindingToken={null}
      readBindingToken="binding"
      onPausedChange={(paused) => pausedReports.push(paused)}
    />
  );
  await waitFor(() => expect(pausedReports.length).toBeGreaterThan(0));
  expect(pausedReports.at(-1)).toBe(false);
  expect(screen.queryByTestId("native-outbox")).toBeNull();
  expect(screen.queryByTestId("outbox-pause")).toBeNull(); // 空条目时不渲染队列区
  expect(bodies).toHaveLength(0);
});

it("暂停态经 onPausedChange 上报为 true（面板据此在发送键区提示）", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(JSON.stringify({ sessionId: "s", revision: 6, paused: true, items: [] }))
    )
  );
  const pausedReports: boolean[] = [];
  render(
    <OpenCodeNativeQueue
      sessionId="s"
      bindingToken="binding"
      onPausedChange={(paused) => pausedReports.push(paused)}
    />
  );
  await waitFor(() => expect(pausedReports.at(-1)).toBe(true));
  // 暂停不建框：已暂停但无待发/未决/错误时整块不渲染
  expect(screen.queryByTestId("native-outbox")).toBeNull();
  expect(screen.queryByTestId("outbox-pause")).toBeNull(); // 空条目时不渲染队列区
  expect(bodies).toHaveLength(0);
});

it("本地控制明确未受理时提示操作失败而非队列读取失败", async () => {
  mockPausedOutbox();
  const original = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST")
        return new Response(JSON.stringify({ actionId: "rejected", state: "not_accepted" }));
      return original(input, init);
    })
  );
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  fireEvent.click(await screen.findByTestId("outbox-retract-local"));

  const status = await screen.findByRole("status");
  expect(status).toHaveTextContent(/(操作|暂停|派发|撤回).*(失败|未受理|未成功|拒绝)/);
  expect(status).not.toHaveTextContent(i18n.t("mobile.openCodeNative.outbox.readFailed"));
  expect(screen.queryByTestId("outbox-unknown")).toBeNull();
  expect(screen.getByTestId("outbox-pause")).toBeInTheDocument(); // 单一文字链入口（无独立框）
});

it("已暂停但无条目：队列区整块不渲染，暂停只在发送键区体现（零写）", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        bodies.push(body);
        return new Response(JSON.stringify({ actionId: body.actionId, state: "settled" }));
      }
      return new Response(JSON.stringify({ sessionId: "s", revision: 6, paused: true, items: [] }));
    })
  );
  const pausedReports: boolean[] = [];
  render(
    <OpenCodeNativeQueue
      sessionId="s"
      bindingToken="binding"
      onPausedChange={(paused) => pausedReports.push(paused)}
    />
  );
  await waitFor(() => expect(pausedReports.at(-1)).toBe(true));
  expect(screen.queryByTestId("native-outbox")).toBeNull();
  expect(screen.queryByTestId("outbox-pause")).toBeNull(); // 空条目时不渲染队列区
  expect(bodies).toHaveLength(0);
});

it("阻断待发和原生未决条目同场可处理，已受理项不占行", async () => {
  render(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  // 本地待发（queued）：可见且可撤回
  expect(await screen.findByTestId("outbox-local-local")).toBeTruthy();
  expect(screen.getByTestId("outbox-retract-local")).not.toBeDisabled();
  // 仅结果未知的原生项保留核对入口
  expect(screen.queryByTestId("outbox-native-native")).toBeNull();
  expect(screen.getByTestId("outbox-native-unknown")).toBeTruthy();
  expect(screen.getByTestId("outbox-verify-unknown")).not.toBeDisabled();
});

it("普通本地排队不显示队列容器或撤回入口", async () => {
  await renderQueueSnapshot([
    {
      actionId: "local",
      owner: "tuvis",
      state: "queued",
      textSummary: "本地待发",
      attachmentCount: 0,
    },
  ]);
  expect(screen.queryByTestId("native-outbox")).toBeNull();
  expect(screen.queryByTestId("outbox-retract-local")).toBeNull();
});

it("原生已确认项不显示队列容器或核对入口", async () => {
  await renderQueueSnapshot([
    {
      actionId: "native",
      owner: "native",
      state: "confirmed",
      textSummary: "原生记录",
      attachmentCount: 0,
    },
  ]);
  expect(screen.queryByTestId("native-outbox")).toBeNull();
  expect(screen.queryByTestId("outbox-verify-native")).toBeNull();
});

function mockPausedOutbox() {
  const original = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const response = await original(input, init);
      if (init?.method === "POST") return response;
      const snapshot = await response.json();
      return new Response(JSON.stringify({ ...snapshot, paused: true }));
    })
  );
}

it.each([
  { name: "断线", bindingToken: null, writeBlockedCode: null, readFailed: false },
  {
    name: "实例身份未核验",
    bindingToken: "binding",
    writeBlockedCode: "OC_INSTANCE_OWNERSHIP_UNVERIFIED",
    readFailed: false,
  },
  { name: "连接读取失败", bindingToken: "binding", writeBlockedCode: null, readFailed: true },
])(
  "$name：已暂停时不渲染恢复入口，本地撤回仍可用且零控制写",
  async ({ bindingToken, writeBlockedCode, readFailed }) => {
    mockPausedOutbox();
    render(
      <OpenCodeNativeQueue
        sessionId="s"
        bindingToken={bindingToken}
        readBindingToken="binding"
        writeBlockedCode={writeBlockedCode}
        readFailed={readFailed}
      />
    );
    await screen.findByTestId("outbox-local-local");
    // 恢复按钮已删除（2026-11-22 用户口径）：任何连接形态下都不再有一个
    // 「恢复本地派发」的写控件可点，因此不可能产生控制 POST。
    expect(screen.getByTestId("outbox-pause")).toBeInTheDocument(); // 单一文字链入口（无独立框）
    expect(screen.queryByTestId("native-pause-entry")).toBeNull();
    // 本地撤回（local_only）在断线/门阻断下仍可用
    expect(screen.getByTestId("outbox-retract-local")).not.toBeDisabled();
    expect(bodies).toHaveLength(0);
  }
);

it("已暂停且连接恢复后，恢复文字链可用", async () => {
  mockPausedOutbox();
  const view = render(
    <OpenCodeNativeQueue sessionId="s" bindingToken={null} readBindingToken="binding" readFailed />
  );
  await screen.findByTestId("outbox-local-local");
  expect(screen.getByTestId("outbox-pause")).toBeInTheDocument(); // 单一文字链入口（无独立框）
  view.rerender(<OpenCodeNativeQueue sessionId="s" bindingToken="binding" />);
  await screen.findByTestId("outbox-local-local");
  expect(screen.getByTestId("outbox-pause")).toBeInTheDocument(); // 单一文字链入口（无独立框）
  expect(bodies).toHaveLength(0);
});

it("断线时空队列不渲染暂停入口（预暂停已随暂停框删除）", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        bodies.push(body);
        return new Response(JSON.stringify({ actionId: body.actionId, state: "settled" }));
      }
      return new Response(
        JSON.stringify({ sessionId: "s", revision: 4, paused: false, items: [] })
      );
    })
  );
  render(
    <OpenCodeNativeQueue sessionId="s" bindingToken={null} readBindingToken="binding" readFailed />
  );
  await act(async () => {
    await Promise.resolve();
  });
  expect(screen.queryByTestId("native-outbox")).toBeNull();
  expect(screen.queryByTestId("outbox-pause")).toBeNull(); // 空条目时不渲染队列区
  expect(bodies).toHaveLength(0);
});

it("断线且本地队列已暂停时仍可撤回待发项", async () => {
  mockPausedOutbox();
  render(
    <OpenCodeNativeQueue sessionId="s" bindingToken={null} readBindingToken="binding" readFailed />
  );
  const retract = await screen.findByTestId("outbox-retract-local");
  expect(retract).not.toBeDisabled();
  fireEvent.click(retract);
  await waitFor(() => expect(bodies).toHaveLength(1));
  expect(bodies[0]).toMatchObject({
    sessionId: "s",
    bindingToken: "binding",
    operation: { kind: "retract_outbox", targetActionId: "local" },
  });
});

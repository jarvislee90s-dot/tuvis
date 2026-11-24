import { dshSession } from "./dshSessionFixture";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { DshActionReceipt, QueueItemView } from "@/mobile/api";
import SessionDetail from "@/mobile/SessionDetail";
import type { Session } from "@/types/session";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

const session = dshSession({
  id: "dsh-f02-session",
  dshInstanceId: "instance-A",
  projectName: "proj",
  projectPath: "/tmp/proj",
  title: "DSH 会话",
  status: "idle",
  lastActivityAt: "2026-11-22T00:00:00Z",
  activeSubagentCount: 0,
});
const otherInstance = { ...session, dshInstanceId: "instance-B" };
const path = "/tmp/proj/notes.md";
const draft = "请整理这份笔记";
const success = /已进入 DSH 会话/;
const firstDeviceScope = "a".repeat(64);
const otherDeviceScope = "b".repeat(64);

function detail(target = session) {
  return <SessionDetail session={target} onBack={() => {}} />;
}

function composerInput() {
  return screen.getByTestId("composer-input");
}

function typeDraft(text: string) {
  fireEvent.change(composerInput(), { target: { value: text } });
}

async function clickSend() {
  const send = screen.getByTestId("composer-send");
  expect(send).toBeEnabled();
  await act(async () => {
    fireEvent.click(send);
  });
}

async function openFileSplit() {
  const inputBefore = composerInput();
  expect(screen.queryByTestId("split-container")).toBeNull();
  await act(async () => {
    fireEvent.click(screen.getByTestId("file-link"));
  });
  const split = screen.getByTestId("split-container");
  expect(split).toHaveAttribute("data-split", "split-h");
  expect(screen.getByTestId("preview-code")).toHaveTextContent("笔记正文");
  expect(inputBefore).not.toBeInTheDocument();
  expect(within(split).getByTestId("composer-input")).toHaveValue(draft);
}

const uploadedPath = "/tmp/proj/.tuvis-attachments/dsh-f02-session/note.txt";

async function attachNote() {
  class AttachmentUpload {
    upload = { onprogress: null };
    onload: (() => void) | null = null;
    status = 200;
    responseText = JSON.stringify({ path: uploadedPath, size: 1 });
    open(method: string, url: string) {
      const request = new URL(url, window.location.href);
      expect(method).toBe("POST");
      expect(request.pathname).toBe("/m/api/v1/session-attachment");
      expect(request.searchParams.get("instanceId")).toBe(session.dshInstanceId);
    }
    setRequestHeader() {}
    send() {
      this.onload?.();
    }
  }
  vi.stubGlobal("XMLHttpRequest", AttachmentUpload);
  await act(async () => {
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [new File(["x"], "note.txt", { type: "text/plain" })] },
    });
  });
  expect(screen.getByText("note.txt")).toBeVisible();
}

async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

type SendBody = {
  actionId: string;
  instanceId: string;
  sessionId: string;
  text: string;
  queueOnly?: boolean;
};

/** 在 HTTP 边界模拟宿主：首次受理先产生实际效果，再丢响应。同 ID 不再产生效果。 */
function installHost(firstResponse: "lost" | "held" = "lost") {
  const posts: SendBody[] = [];
  const postScopes: Array<string | null> = [];
  const preflightScopes: string[] = [];
  const hostEffects: SendBody[] = [];
  const effectScopes = new Map<SendBody, string>();
  const reads: Array<{ actionId: string; reply: () => void }> = [];
  let replyFirstPost: (() => void) | undefined;
  let queueItems: QueueItemView[] = [];
  let disabledPreflight: 200 | 404 | null = null;
  document.cookie = "mam_device=f02-first; path=/";
  function receipt(action: SendBody, state: DshActionReceipt["state"]): DshActionReceipt {
    return {
      actionId: action.actionId,
      state,
      nativeId: `native-${action.actionId}`,
      reasonCode: null,
    };
  }
  function postResult(action: SendBody, duplicate = false) {
    return Response.json({ status: "dsh", receipt: receipt(action, "accepted"), duplicate });
  }
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), window.location.href);
      // 浏览器同源请求自动携带 Cookie；fetch 桩从同一个 Cookie jar 模拟服务端设备归属。
      const deviceScope = document.cookie.split(/;\s*/).includes("mam_device=f02-other")
        ? otherDeviceScope
        : firstDeviceScope;
      const actionPath = /^\/m\/api\/v1\/dsh\/action\/([^/]+)$/.exec(url.pathname);
      if (actionPath) {
        expect(init?.method ?? "GET").toBe("GET");
        const actionId = decodeURIComponent(actionPath[1]);
        const action = hostEffects.find(
          (entry) => entry.actionId === actionId && effectScopes.get(entry) === deviceScope
        );
        // 挂住 GET，使点击重按发生在宿主确认之前；手动释放真实格式的证据。
        return new Promise<Response>((resolve) =>
          reads.push({
            actionId,
            reply: () =>
              resolve(
                action
                  ? Response.json({
                      receipt: receipt(action, "succeeded"),
                      target: { hostId: action.instanceId, epoch: 1, sessionId: action.sessionId },
                      updatedAtMs: Date.now(),
                      evidence: {
                        operation: "send",
                        nativeId: `native-${actionId}`,
                        epoch: 1,
                        effectKind: "message",
                      },
                    })
                  : Response.json({ error: "not_found" }, { status: 404 })
              ),
          })
        );
      }
      switch (url.pathname.split("/").at(-1)) {
        case "session-send": {
          expect(init?.method).toBe("POST");
          const body = JSON.parse(String(init?.body)) as SendBody;
          const scope = new Headers(init?.headers).get("x-tuvis-dsh-retry-scope");
          posts.push(body);
          postScopes.push(scope);
          // 可选新头：旧客户端不带头仍可发；若携带的 scope 与 Cookie 不符，先拒绝再查账。
          if (scope !== null && scope !== deviceScope) {
            return Response.json({ error: "dsh_retry_scope_changed" }, { status: 409 });
          }
          const existing = hostEffects.find(
            (effect) =>
              effect.actionId === body.actionId && effectScopes.get(effect) === deviceScope
          );
          if (existing) {
            if (
              existing.instanceId !== body.instanceId ||
              existing.sessionId !== body.sessionId ||
              existing.text !== body.text ||
              !!existing.queueOnly !== !!body.queueOnly
            ) {
              return Response.json({ error: "action_conflict" }, { status: 409 });
            }
            return postResult(existing, true);
          }
          hostEffects.push(body);
          effectScopes.set(body, deviceScope);
          if (posts.length === 1) {
            if (firstResponse === "lost") throw new TypeError("宿主受理后连接断开");
            return new Promise<Response>((resolve) => {
              replyFirstPost = () => resolve(postResult(body));
            });
          }
          return postResult(body);
        }
        case "session-send-info":
          preflightScopes.push(deviceScope);
          if (disabledPreflight === 404) {
            return Response.json(
              { error: "no_session", dshRetryScope: deviceScope },
              { status: 404 }
            );
          }
          if (disabledPreflight === 200) {
            return Response.json({
              injectable: false,
              reasonCode: "DSH_SWITCH_OFF",
              dshRetryScope: deviceScope,
            });
          }
          return Response.json({
            injectable: true,
            channels: ["dsh_desktop"],
            visibility: "realtime",
            dshRetryScope: deviceScope,
          });
        case "session-messages":
          return Response.json({
            messages: [
              {
                seq: 1,
                role: "assistant",
                kind: "assistant",
                content: `请查看 ${path}`,
                ts: null,
                collapsed: false,
              },
            ],
            truncated: false,
          });
        case "session-files":
          return Response.json({
            files: [
              {
                path,
                fileKey: "notes",
                lastSeq: 1,
                lastTs: 1000,
                hits: 1,
                modified: false,
                origin: "tool_write",
              },
            ],
            truncated: false,
          });
        case "file":
          return Response.json({ content: "笔记正文", mime: "text/plain" });
        case "session-queue":
          return Response.json({ items: queueItems });
        case "retract":
          queueItems = [];
          return Response.json({ ok: true });
        case "session-subagents":
          return Response.json({ subagents: [] });
        case "session-approve-options":
          return Response.json({ available: false });
        case "session-question":
          return Response.json({ available: false, questions: [] });
        default:
          return Response.json({ error: "not_found" }, { status: 404 });
      }
    })
  );
  return {
    posts,
    postScopes,
    preflightScopes,
    hostEffects,
    reads,
    disableWrites: (status: 200 | 404) => {
      disabledPreflight = status;
    },
    switchPairedDevice: () => {
      document.cookie = "mam_device=f02-other; path=/";
    },
    setQueue: (items: QueueItemView[]) => {
      queueItems = items;
    },
    finishFirstPost: async () => {
      expect(replyFirstPost).toBeTypeOf("function");
      await act(async () => {
        replyFirstPost!();
      });
    },
  };
}

type Host = ReturnType<typeof installHost>;

async function sendAndLoseResponse(host: Host, expectedText = draft) {
  typeDraft(draft);
  await clickSend();
  expect(screen.getByText(/待核实/)).toBeVisible();
  expect(composerInput()).toHaveValue(draft);
  expect(host.posts).toEqual([
    expect.objectContaining({
      actionId: expect.any(String),
      instanceId: session.dshInstanceId,
      sessionId: session.id,
      text: expectedText,
    }),
  ]);
  expect(host.posts[0].actionId).not.toBe("");
  expect(host.hostEffects).toEqual(host.posts);
  return host.posts[0];
}

async function expectSameActionOnRetry(host: Host, original: SendBody) {
  await clickSend();
  // 先核对所有 POST 的 ID 和载荷，再核对宿主效果；也允许只 GET、不 POST。
  expect([...new Set(host.posts.map(({ actionId }) => actionId))]).toEqual([original.actionId]);
  for (const body of host.posts) expect(body).toEqual(original);
  expect(host.hostEffects).toEqual([original]);
  await advance(1_000);
  expect(host.reads.length).toBeGreaterThan(0);
  expect(host.reads.every(({ actionId }) => actionId === original.actionId)).toBe(true);
  await act(async () => {
    host.reads.at(-1)!.reply();
  });
  expect(screen.getByText(success)).toBeVisible();
  expect(host.hostEffects).toEqual([original]);
}

let unpinZh: () => void;
beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});
afterAll(() => {
  unpinZh();
});

beforeEach(() => {
  sessionStorage.clear();
  MockEventSource.reset();
  vi.useFakeTimers();
  vi.stubGlobal("EventSource", MockEventSource);
  // 宽竖屏默认保持聊天框，点击正文文件后才切到左右分屏。
  vi.stubGlobal("innerWidth", 820);
  vi.stubGlobal("innerHeight", 1180);
  vi.stubGlobal("matchMedia", (media: string) => ({
    media,
    matches: media.includes("min-width") || media.includes("portrait"),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  }));
});

afterEach(() => {
  cleanup();
  document.cookie = "mam_device=; max-age=0; path=/";
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("SessionDetail：F02 DSH 未决发送", () => {
  it("打开文件分屏重挂后重按保留的草稿，只确认原动作且宿主只接到一条", async () => {
    const host = installHost();
    await act(async () => {
      render(detail());
    });
    const original = await sendAndLoseResponse(host);
    await openFileSplit();
    expect(host.posts).toEqual([original]);

    await expectSameActionOnRetry(host, original);
  });

  it("同标签页刷新后主动重按同文，仍只确认原动作", async () => {
    const host = installHost();
    const view = render(detail());
    await act(async () => {});
    const original = await sendAndLoseResponse(host);
    view.unmount();
    // 整页重新挂载，沿用真实 sessionStorage，不注入任何应用专用键。
    await act(async () => {
      render(detail());
    });
    expect(host.posts).toEqual([original]);
    // 不规定刷新必须恢复草稿正文；即使重输未确认的同文，也不能变成第二条。
    if ((composerInput() as HTMLTextAreaElement).value === "") typeDraft(draft);
    expect(composerInput()).toHaveValue(draft);
    await expectSameActionOnRetry(host, original);
  });

  it.each([200, 404] as const)("写门关闭并重新挂载时只核对旧动作（预检 %i）", async (status) => {
    const host = installHost();
    const view = render(detail());
    await act(async () => {});
    const original = await sendAndLoseResponse(host);
    view.unmount();
    host.disableWrites(status);
    await act(async () => {
      render(detail());
    });
    await advance(1_000);
    expect(screen.getByText(/待核实/)).toBeVisible();
    expect(composerInput()).toBeDisabled();
    expect([...new Set(host.reads.map(({ actionId }) => actionId))]).toEqual([original.actionId]);
    expect(host.posts).toEqual([original]);
  });

  it("同一实例和会话切到不同配对设备 scope 后，明确发送同文会用新 ID", async () => {
    const host = installHost();
    const firstDevice = render(detail());
    await act(async () => {});
    const original = await sendAndLoseResponse(host);
    firstDevice.unmount();

    // 模拟在同一标签页重新配对。存储与 DSH 目标均不变，只有 send-info 的 scope 变更。
    host.switchPairedDevice();
    await act(async () => {
      render(detail());
    });
    expect(host.posts).toEqual([original]);
    typeDraft(draft);
    await clickSend();
    expect(host.posts).toHaveLength(2);
    expect(host.posts[1]).toMatchObject({
      instanceId: original.instanceId,
      sessionId: original.sessionId,
      text: original.text,
    });
    expect(host.posts[1].actionId).not.toBe(original.actionId);
    expect(host.hostEffects).toEqual(host.posts);
  });

  it("同一输入区的配对 Cookie 换代时拒绝旧 scope，刷新预检后才按新 scope 发送", async () => {
    const host = installHost();
    await act(async () => {
      render(detail());
    });
    const original = await sendAndLoseResponse(host);
    const input = composerInput();
    expect(host.preflightScopes.at(-1)).toBe(firstDeviceScope);
    host.switchPairedDevice();
    expect(composerInput()).toBe(input);
    expect(input).toHaveValue(draft);

    await clickSend();
    // 服务端必须在账本/宿主效果之前拒绝旧头，不能把原动作 ID 在新设备下当成首发。
    expect(host.hostEffects).toEqual([original]);
    expect(host.posts).toEqual([original, original]);
    expect(host.postScopes).toEqual([firstDeviceScope, firstDeviceScope]);
    expect(input).toHaveValue(draft);

    const preflightsBeforeRefresh = host.preflightScopes.length;
    await act(async () => {
      fireEvent.click(screen.getByTestId("detail-refresh"));
    });
    expect(host.preflightScopes.slice(preflightsBeforeRefresh)).toContain(otherDeviceScope);
    expect(composerInput()).toBe(input);
    await clickSend();
    expect(host.posts).toHaveLength(3);
    expect(host.posts[2]).toMatchObject({
      instanceId: original.instanceId,
      sessionId: original.sessionId,
      text: original.text,
    });
    expect(host.posts[2].actionId).not.toBe(original.actionId);
    expect(host.postScopes).toEqual([firstDeviceScope, firstDeviceScope, otherDeviceScope]);
    expect(host.hostEffects).toEqual([original, host.posts[2]]);
  });

  it("旧版存储只留待查 ID、没有载荷摘要时，主动重按只查账而不 POST", async () => {
    const host = installHost();
    const legacyActionId = "0f000000-0000-4000-8000-000000000001";
    // 以旧版真实写入格式构造升级现场，不写入任何新版摘要/作用域字段。
    const storageKey = `dsh-send-receipts:${JSON.stringify([session.dshInstanceId, session.id])}`;
    sessionStorage.setItem(
      storageKey,
      JSON.stringify([{ actionId: legacyActionId, identityUnverified: false }])
    );
    await act(async () => {
      render(detail());
    });
    expect(screen.getByText(/待核实/)).toBeVisible();
    typeDraft(draft);
    await clickSend();
    await advance(1_000);

    expect(host.posts).toEqual([]);
    expect(host.hostEffects).toEqual([]);
    expect([...new Set(host.reads.map(({ actionId }) => actionId))]).toEqual([legacyActionId]);
    expect(composerInput()).toHaveValue(draft);
  });

  it("同一会话 ID 的另一实例明确发送同文，不复用原实例的锚", async () => {
    const host = installHost();
    const view = render(detail());
    await act(async () => {});
    const original = await sendAndLoseResponse(host);
    await act(async () => {
      view.rerender(detail(otherInstance));
    });
    expect(host.posts).toEqual([original]);
    typeDraft(draft);
    await clickSend();
    expect(host.posts).toHaveLength(2);
    expect(host.posts[1]).toMatchObject({
      instanceId: otherInstance.dshInstanceId,
      sessionId: session.id,
      text: draft,
    });
    expect(host.posts[1].actionId).not.toBe(original.actionId);
    expect(host.hostEffects).toEqual(host.posts);
  });

  it("未决期间改正文，再主动发送时使用新动作而非旧 ID", async () => {
    const host = installHost();
    await act(async () => {
      render(detail());
    });
    const original = await sendAndLoseResponse(host);
    typeDraft(`${draft}，只保留三条`);
    await clickSend();
    expect(host.posts).toHaveLength(2);
    expect(host.posts[1]).toMatchObject({
      instanceId: session.dshInstanceId,
      text: `${draft}，只保留三条`,
    });
    expect(host.posts[1].actionId).not.toBe(original.actionId);
    expect(host.hostEffects).toEqual(host.posts);
  });

  it("正文相同但改为只入队，按不同载荷创建新动作", async () => {
    const host = installHost();
    host.setQueue([
      { id: 7, content: `${draft} [mobile 另一设备]`, enqueuedAt: 1000, position: 1 },
    ]);
    await act(async () => {
      render(detail());
    });
    const original = await sendAndLoseResponse(host);
    expect(original.queueOnly).toBeUndefined();
    await act(async () => {
      fireEvent.click(within(screen.getByTestId("queue-row-7")).getByTestId("queue-edit"));
    });
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
    expect(composerInput()).toHaveValue(draft);

    await clickSend();
    expect(host.posts).toHaveLength(2);
    expect(host.posts[1]).toMatchObject({ text: original.text, queueOnly: true });
    expect(host.posts[1].actionId).not.toBe(original.actionId);
    expect(host.hostEffects).toEqual(host.posts);
  });

  it("只入队的 POST 丢响应后，未改草稿直接重按保留原 ID 和 queueOnly", async () => {
    const host = installHost();
    host.setQueue([
      { id: 7, content: `${draft} [mobile 另一设备]`, enqueuedAt: 1000, position: 1 },
    ]);
    await act(async () => {
      render(detail());
    });
    await act(async () => {
      fireEvent.click(within(screen.getByTestId("queue-row-7")).getByTestId("queue-edit"));
    });
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
    expect(composerInput()).toHaveValue(draft);
    await clickSend();
    expect(screen.getByText(/待核实/)).toBeVisible();
    expect(host.posts).toEqual([
      expect.objectContaining({ text: draft, queueOnly: true, actionId: expect.any(String) }),
    ]);
    const original = host.posts[0];

    expect(composerInput()).toHaveValue(draft);
    await clickSend();
    expect(host.posts).toEqual([original, original]);
    expect(host.hostEffects).toEqual([original]);
  });

  it("正文不变而新增附件标记时，按不同载荷创建新动作", async () => {
    const host = installHost();
    await act(async () => {
      render(detail());
    });
    const original = await sendAndLoseResponse(host);
    await attachNote();
    expect(composerInput()).toHaveValue(draft);

    await clickSend();
    expect(host.posts).toHaveLength(2);
    expect(host.posts[1].text).toBe(`${draft}\n<file path="${uploadedPath}">`);
    expect(host.posts[1].actionId).not.toBe(original.actionId);
    expect(host.hostEffects).toEqual(host.posts);
  });

  it("分屏重挂丢失附件标记后，重按相同正文不会创建新动作", async () => {
    const host = installHost();
    await act(async () => {
      render(detail());
    });
    await attachNote();
    const original = await sendAndLoseResponse(host, `${draft}\n<file path="${uploadedPath}">`);
    expect(screen.getByTestId(/^attach-chip-/)).toBeVisible();

    await openFileSplit();
    // 不是用户主动移除附件；重挂重建了 Composer，正文仍然是同一份。
    expect(screen.queryByTestId(/^attach-chip-/)).toBeNull();
    expect(host.posts).toEqual([original]);

    await expectSameActionOnRetry(host, original);
  });

  it("GET 已确认成功后，未编辑的原草稿不会在重按时静默新发", async () => {
    const host = installHost();
    await act(async () => {
      render(detail());
    });
    const original = await sendAndLoseResponse(host);
    await advance(1_000);
    expect(host.reads.map(({ actionId }) => actionId)).toEqual([original.actionId]);
    await act(async () => {
      host.reads[0].reply();
    });
    expect(screen.getByText(success)).toBeVisible();
    expect(host.posts).toEqual([original]);

    const input = composerInput() as HTMLTextAreaElement;
    const button = screen.getByTestId("composer-send");
    if (input.value === "") {
      expect(button).toBeDisabled();
    } else {
      expect(input).toHaveValue(draft);
      // 未清稿的实现也可以只查账或沿原动作重放；不要人为重新输入同样的文字。
      await act(async () => {
        fireEvent.click(button);
      });
      await advance(1_000);
    }
    expect([...new Set(host.posts.map(({ actionId }) => actionId))]).toEqual([original.actionId]);
    for (const body of host.posts) expect(body).toEqual(original);
    expect([...new Set(host.reads.map(({ actionId }) => actionId))]).toEqual([original.actionId]);
    expect(host.hostEffects).toEqual([original]);
  });

  it("旧动作经账本确认后，重新输入同文并主动发送会创建新动作", async () => {
    const host = installHost();
    await act(async () => {
      render(detail());
    });
    const original = await sendAndLoseResponse(host);
    await advance(1_000);
    expect(host.reads.map(({ actionId }) => actionId)).toEqual([original.actionId]);
    await act(async () => {
      host.reads[0].reply();
    });
    expect(screen.getByText(success)).toBeVisible();
    expect(host.posts).toEqual([original]);
    typeDraft("");
    typeDraft(draft);
    await clickSend();
    expect(host.posts).toHaveLength(2);
    expect(host.posts[1].text).toBe(original.text);
    expect(host.posts[1].actionId).not.toBe(original.actionId);
    expect(host.hostEffects).toEqual(host.posts);
  });

  it("旧账本成功不清掉等待期间编辑的新稿", async () => {
    const host = installHost();
    await act(async () => {
      render(detail());
    });
    const original = await sendAndLoseResponse(host);
    await advance(1_000);
    expect(host.reads.map(({ actionId }) => actionId)).toEqual([original.actionId]);
    typeDraft("尚未提交的新稿");
    await act(async () => {
      host.reads[0].reply();
    });
    expect(screen.getByText(success)).toBeVisible();
    expect(composerInput()).toHaveValue("尚未提交的新稿");
    expect(host.hostEffects).toEqual([original]);
  });

  it("迟到的旧 POST 受理回执不清掉等待期间编辑的新稿", async () => {
    const host = installHost("held");
    await act(async () => {
      render(detail());
    });
    typeDraft(draft);
    await clickSend();
    expect(screen.getByTestId("send-receipt-delivering")).toBeVisible();
    typeDraft("在发送期间写下的新稿");
    await host.finishFirstPost();
    expect(screen.getByText(/DSH 已受理/)).toBeVisible();
    expect(composerInput()).toHaveValue("在发送期间写下的新稿");
    expect(host.hostEffects).toEqual([expect.objectContaining({ text: draft })]);
  });
});

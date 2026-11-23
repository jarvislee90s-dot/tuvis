// 本地控制动作的跨绑定 / 跨会话隔离锁。
//
// 2026-11-22 用户口径：暂停/恢复按钮已从队列区删除（暂停只在发送键上体现），
// 因此本文件的「本地控制动作」载体改为**撤回本地待发**（`retract_outbox`）——
// 它同样是 local_only（不需要服务归属）、同样落控制锚、同样单飞 + 只读核对，
// 所有「零重投 / POST 次数」断言原样保留。
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import OpenCodeNativeQueue, { resetQueueControlMemoryForTests } from "@/mobile/OpenCodeNativeQueue";
import i18n from "@/i18n";

let sessionNumber = 0;
let sessionId: string;
let key: string;
const json = (body: unknown) => new Response(JSON.stringify(body));
let posts: Record<string, unknown>[];
let releases: Array<(response: Response) => void>;
let actionReads: string[];
let queueReads: (string | null)[];
let confirmedVerifications: Set<string>;

beforeEach(async () => {
  sessionId = `isolation-${++sessionNumber}`;
  key = `tuvis.opencodeNative.outboxControl.${sessionId}`;
  await i18n.changeLanguage("zh");
  sessionStorage.clear();
  posts = [];
  releases = [];
  actionReads = [];
  queueReads = [];
  confirmedVerifications = new Set();
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/opencode/actions") && init?.method === "POST") {
        posts.push(JSON.parse(String(init.body)) as Record<string, unknown>);
        return new Promise<Response>((resolve) => releases.push(resolve));
      }
      if (url.includes("/opencode/actions/")) {
        actionReads.push(url);
        const match = url.match(/\/actions\/([^/]+)(?:\/verify)?$/);
        const actionId = match?.[1];
        const state =
          url.endsWith("/verify") && actionId && confirmedVerifications.has(actionId)
            ? "confirmed"
            : "unconfirmed";
        return Promise.resolve(json({ actionId, state }));
      }
      const binding = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
      queueReads.push(binding);
      // 带阻断原因的待发项：本地行常驻（撤回按钮是本文件里的控制动作载体）。
      return Promise.resolve(
        json({
          sessionId,
          revision: 1,
          paused: false,
          items: [
            {
              actionId: binding,
              owner: "tuvis",
              state: "queued",
              reasonCode: "OC_BINDING_STALE",
              textSummary: `队列 ${binding}`,
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
  sessionStorage.clear();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function queue(binding: string) {
  return (
    <OpenCodeNativeQueue sessionId={sessionId} bindingToken={binding} readBindingToken={binding} />
  );
}

/** 控制按钮定位：撤回该绑定下的本地待发项（local_only 控制动作）。 */
function controlButton(binding: string) {
  return screen.queryByTestId(`outbox-retract-${binding}`);
}

async function expectQueueRead(binding: string) {
  await waitFor(() => expect(queueReads.at(-1)).toBe(binding));
  // 正向标记：该绑定的本地待发行已落地（证明 queueReads 不是空跑）
  expect(await screen.findByTestId(`outbox-local-${binding}`)).toBeInTheDocument();
}

async function startA() {
  const view = render(queue("A"));
  await expectQueueRead("A");
  const control = controlButton("A");
  expect(control).not.toBeNull();
  fireEvent.click(control!);
  await waitFor(() => expect(posts).toHaveLength(1));
  return view;
}

it("控制提交前发现无通知写入的未知锚时阻止提交并提供核对入口", async () => {
  render(queue("A"));
  await expectQueueRead("A");
  expect(controlButton("A")).not.toBeDisabled();
  expect(screen.queryByTestId("outbox-unknown")).not.toBeInTheDocument();
  expect(sessionStorage.getItem(key)).toBeNull();
  expect(sessionStorage.getItem(`${key}.ids`)).toBeNull();

  const oldId = "external-unknown-control";
  sessionStorage.setItem(`${key}.ids`, JSON.stringify([oldId]));
  sessionStorage.setItem(key, oldId);
  fireEvent.click(controlButton("A")!);

  expect.soft(posts).toHaveLength(0);
  expect.soft(sessionStorage.getItem(key)).toBe(oldId);
  const unknown = screen.queryByTestId("outbox-unknown");
  expect(unknown).not.toBeNull();
  const verify = within(unknown!).getByRole("button", {
    name: i18n.t("mobile.openCodeNative.outbox.verify"),
  });
  fireEvent.click(verify);
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${oldId}/verify`))).toBe(true)
  );
  expect(controlButton("A")).toBeDisabled();
});

it("控制提交前发现已知属于 B 的锚时允许 A 提交", async () => {
  const view = render(queue("B"));
  await expectQueueRead("B");
  fireEvent.click(controlButton("B")!);
  await waitFor(() => expect(posts).toHaveLength(1));
  const idB = String(posts[0].actionId);
  view.unmount();
  sessionStorage.removeItem(key);
  sessionStorage.removeItem(`${key}.ids`);

  render(queue("A"));
  await expectQueueRead("A");
  expect(controlButton("A")).not.toBeDisabled();
  expect(screen.queryByTestId("outbox-unknown")).not.toBeInTheDocument();
  sessionStorage.setItem(`${key}.ids`, JSON.stringify([idB]));
  sessionStorage.setItem(key, idB);
  fireEvent.click(controlButton("A")!);

  await waitFor(() => expect(posts).toHaveLength(2));
  expect(posts[1]).toMatchObject({
    bindingToken: "A",
    operation: { kind: "retract_outbox", targetActionId: "A" },
  });
});

it("A 控制未决时切到 B，B 可以独立发起控制", async () => {
  const view = await startA();
  view.rerender(queue("B"));
  await expectQueueRead("B");
  expect(controlButton("B")).not.toBeDisabled();
  fireEvent.click(controlButton("B")!);
  await waitFor(() => expect(posts).toHaveLength(2));
  expect(posts[1]).toMatchObject({
    bindingToken: "B",
    operation: { kind: "retract_outbox", targetActionId: "B" },
  });
});

it("A 到 B 再回 A 时恢复 A 的只读核对入口", async () => {
  const view = await startA();
  const id = String(posts[0].actionId);
  await act(async () => releases[0](json({ actionId: id, state: "unconfirmed" })));
  view.rerender(queue("B"));
  await expectQueueRead("B");
  view.rerender(queue("A"));
  await expectQueueRead("A");
  // 未决锚锁住 A 的本地控制，但只读核对入口在场
  expect(controlButton("A")).toBeDisabled();
  const unknown = screen.getByTestId("outbox-unknown");
  fireEvent.click(
    within(unknown).getByRole("button", { name: i18n.t("mobile.openCodeNative.outbox.verify") })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${id}/verify`))).toBe(true)
  );
  expect(posts).toHaveLength(1);
});

it("旧 A 控制回执迟到时不删除新 B 的 actionId 锚", async () => {
  const view = await startA();
  view.unmount();
  // 模拟旧界面已显式放弃 A 的本地核对，再由 B 发起独立动作。
  sessionStorage.removeItem(key);
  render(queue("B"));
  await expectQueueRead("B");
  fireEvent.click(controlButton("B")!);
  await waitFor(() => expect(posts).toHaveLength(2));
  const idB = String(posts[1].actionId);
  expect(sessionStorage.getItem(key)).toBe(idB);
  await act(async () => releases[0](json({ actionId: posts[0].actionId, state: "confirmed" })));
  expect(sessionStorage.getItem(key)).toBe(idB);
});

it("切到 B 后旧 A 的拒绝回执不显示在 B", async () => {
  const view = await startA();
  view.rerender(queue("B"));
  await expectQueueRead("B");
  await act(async () => releases[0](json({ actionId: posts[0].actionId, state: "not_accepted" })));
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
  expect(controlButton("B")).not.toBeDisabled();
  expect(screen.queryByTestId("outbox-unknown")).not.toBeInTheDocument();
  expect(posts).toHaveLength(1);
});

it("存储可读但写入失败时，A 未决切换 B 再回 A 仍阻止重复控制", async () => {
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new DOMException("quota exceeded", "QuotaExceededError");
  });
  const view = await startA();
  expect(sessionStorage.getItem(key)).toBeNull();
  view.rerender(queue("B"));
  await expectQueueRead("B");
  view.rerender(queue("A"));
  await expectQueueRead("A");
  expect(controlButton("A")).toBeDisabled();
  expect(posts).toHaveLength(1);
});

it("新动作持久化成功后立即读取失败，切换绑定再返回仍阻止重发", async () => {
  const setItem = Storage.prototype.setItem;
  const getItem = Storage.prototype.getItem;
  let readFailed = false;
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(function (this: Storage, itemKey) {
    if (readFailed) throw new DOMException("storage unavailable", "SecurityError");
    return getItem.call(this, itemKey);
  });
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (
    this: Storage,
    itemKey,
    value
  ) {
    setItem.call(this, itemKey, value);
    if (itemKey === key) readFailed = true;
  });
  const view = await startA();
  expect(readFailed).toBe(true);
  view.rerender(queue("B"));
  await expectQueueRead("B");
  view.rerender(queue("A"));
  await expectQueueRead("A");
  expect(controlButton("A")).toBeDisabled();
  expect(posts).toHaveLength(1);
});

it("首次恢复时主锚读取失败仍保留已读历史且不覆盖持久历史", async () => {
  const first = "partial-read-first";
  const second = "partial-read-second";
  const history = JSON.stringify([first, second]);
  sessionStorage.setItem(`${key}.ids`, history);
  sessionStorage.setItem(key, "unread-primary");
  confirmedVerifications.add(first);
  const getItem = Storage.prototype.getItem;
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(function (this: Storage, itemKey) {
    if (itemKey === key) throw new DOMException("storage unavailable", "SecurityError");
    return getItem.call(this, itemKey);
  });
  const setItem = vi.spyOn(Storage.prototype, "setItem");
  const removeItem = vi.spyOn(Storage.prototype, "removeItem");
  render(queue("A"));
  await expectQueueRead("A");
  expect(controlButton("A")).toBeDisabled();
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${first}/verify`))).toBe(true)
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${second}`))).toBe(true)
  );
  expect(controlButton("A")).toBeDisabled();
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${second}/verify`))).toBe(true)
  );
  expect(setItem).not.toHaveBeenCalled();
  expect(removeItem).not.toHaveBeenCalled();
  expect(sessionStorage.getItem(`${key}.ids`)).toBe(history);
  expect(getItem.call(sessionStorage, key)).toBe("unread-primary");
  expect(posts).toHaveLength(0);
});

it("主锚读取失败时已知历史全部确认仍锁定，重试读取后核对遗漏主锚才恢复控制", async () => {
  const first = "partial-complete-first";
  const second = "partial-complete-second";
  const third = "partial-complete-primary";
  sessionStorage.setItem(`${key}.ids`, JSON.stringify([first, second]));
  sessionStorage.setItem(key, third);
  confirmedVerifications.add(first);
  confirmedVerifications.add(second);
  confirmedVerifications.add(third);
  const getItem = Storage.prototype.getItem;
  let primaryUnavailable = true;
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(function (this: Storage, itemKey) {
    if (itemKey === key && primaryUnavailable) {
      throw new DOMException("storage unavailable", "SecurityError");
    }
    return getItem.call(this, itemKey);
  });

  render(queue("A"));
  await expectQueueRead("A");
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${first}/verify`))).toBe(true)
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${second}`))).toBe(true)
  );
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${second}/verify`))).toBe(true)
  );
  expect(controlButton("A")).toBeDisabled();
  expect(posts).toHaveLength(0);

  const retry = await screen.findByRole("button", { name: /重新读取/ });
  fireEvent.click(retry);
  expect(controlButton("A")).toBeDisabled();
  expect(posts).toHaveLength(0);
  primaryUnavailable = false;
  fireEvent.click(retry);
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${third}`))).toBe(true)
  );
  expect(controlButton("A")).toBeDisabled();
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${third}/verify`))).toBe(true)
  );
  await waitFor(() => expect(controlButton("A")).not.toBeDisabled());
  fireEvent.click(controlButton("A")!);
  await waitFor(() => expect(posts).toHaveLength(1));
});

it("刷新后第一条未知归属锚已确认，第二条仍阻止重发且可核对", async () => {
  sessionStorage.setItem(`${key}.ids`, JSON.stringify(["unknown-first", "unknown-second"]));
  sessionStorage.setItem(key, "unknown-second");
  confirmedVerifications.add("unknown-first");
  render(queue("A"));
  await expectQueueRead("A");
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith("/actions/unknown-first/verify"))).toBe(true)
  );
  await waitFor(() =>
    expect(sessionStorage.getItem(`${key}.ids`)).toBe(JSON.stringify(["unknown-second"]))
  );
  expect(controlButton("A")).toBeDisabled();
  expect(posts).toHaveLength(0);
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith("/actions/unknown-second/verify"))).toBe(true)
  );
});

it.each(["切换绑定", "重挂"])(
  "刷新读取多个未知锚后存储读取失败，%s 仍阻断控制并允许逐条核对",
  async (transition) => {
    const first = "read-failure-first";
    const second = "read-failure-second";
    sessionStorage.setItem(`${key}.ids`, JSON.stringify([first, second]));
    sessionStorage.setItem(key, second);
    confirmedVerifications.add(first);
    const view = render(queue("A"));
    await expectQueueRead("A");
    expect(controlButton("A")).toBeDisabled();

    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("storage unavailable", "SecurityError");
    });
    if (transition === "切换绑定") {
      view.rerender(queue("B"));
    } else {
      view.unmount();
      render(queue("B"));
    }
    await expectQueueRead("B");
    expect(controlButton("B")).toBeDisabled();
    expect(posts).toHaveLength(0);
    fireEvent.click(
      within(screen.getByTestId("outbox-unknown")).getByRole("button", {
        name: i18n.t("mobile.openCodeNative.outbox.verify"),
      })
    );
    await waitFor(() =>
      expect(actionReads.some((url) => url.endsWith(`/actions/${first}/verify`))).toBe(true)
    );
    expect(controlButton("B")).toBeDisabled();
    await waitFor(() =>
      expect(actionReads.some((url) => url.endsWith(`/actions/${second}`))).toBe(true)
    );
    fireEvent.click(
      within(screen.getByTestId("outbox-unknown")).getByRole("button", {
        name: i18n.t("mobile.openCodeNative.outbox.verify"),
      })
    );
    await waitFor(() =>
      expect(actionReads.some((url) => url.endsWith(`/actions/${second}/verify`))).toBe(true)
    );
    expect(posts).toHaveLength(0);
  }
);

it("未知锚核对完成时读取失败，不得覆盖持久化中的其他未知历史", async () => {
  const first = "cleanup-failure-first";
  const second = "cleanup-failure-second";
  sessionStorage.setItem(`${key}.ids`, JSON.stringify([first, second]));
  sessionStorage.setItem(key, second);
  confirmedVerifications.add(first);
  render(queue("A"));
  await expectQueueRead("A");
  const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new DOMException("storage unavailable", "SecurityError");
  });
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith(`/actions/${first}/verify`))).toBe(true)
  );
  getItem.mockRestore();
  expect(sessionStorage.getItem(`${key}.ids`)).toBe(JSON.stringify([first, second]));
  expect(sessionStorage.getItem(key)).toBe(second);
  expect(controlButton("A")).toBeDisabled();
  expect(posts).toHaveLength(0);
});

it("已成功持久化删除的锚再次出现时重新阻断并允许核对", async () => {
  const id = "successfully-removed-reappears";
  sessionStorage.setItem(`${key}.ids`, JSON.stringify([id]));
  sessionStorage.setItem(key, id);
  confirmedVerifications.add(id);
  const view = render(queue("A"));
  await expectQueueRead("A");
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() => expect(sessionStorage.getItem(`${key}.ids`)).toBeNull());
  expect(sessionStorage.getItem(key)).toBeNull();
  view.unmount();
  sessionStorage.setItem(`${key}.ids`, JSON.stringify([id]));
  sessionStorage.setItem(key, id);
  confirmedVerifications.delete(id);
  render(queue("B"));
  await expectQueueRead("B");
  expect(controlButton("B")).toBeDisabled();
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.filter((url) => url.endsWith(`/actions/${id}/verify`))).toHaveLength(2)
  );
  expect(posts).toHaveLength(0);
});

it("回到 A 后旧 A 拒绝回执收敛时重读剩余锚但不显示旧通知", async () => {
  const view = await startA();
  const oldId = String(posts[0].actionId);
  sessionStorage.setItem(`${key}.ids`, JSON.stringify([oldId, "remaining-unknown"]));
  sessionStorage.setItem(key, "remaining-unknown");
  view.rerender(queue("B"));
  await expectQueueRead("B");
  view.rerender(queue("A"));
  await expectQueueRead("A");
  await act(async () => releases[0](json({ actionId: oldId, state: "not_accepted" })));
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
  expect(controlButton("A")).toBeDisabled();
  fireEvent.click(
    within(screen.getByTestId("outbox-unknown")).getByRole("button", {
      name: i18n.t("mobile.openCodeNative.outbox.verify"),
    })
  );
  await waitFor(() =>
    expect(actionReads.some((url) => url.endsWith("/actions/remaining-unknown/verify"))).toBe(true)
  );
  expect(posts).toHaveLength(1);
});

it("刷新后未知归属的旧锚不猜为当前 B 的绑定并保守阻止重发", async () => {
  sessionStorage.setItem(key, "old-control");
  render(queue("B"));
  await expectQueueRead("B");
  expect(screen.getByTestId("outbox-unknown")).toBeInTheDocument();
  expect(controlButton("B")).toBeDisabled();
  expect(posts).toHaveLength(0);
  expect(sessionStorage.getItem(key)).toBe("old-control");
});

// 任务 6：附件图片懒加载与资源生命周期锁。
// 覆盖：无视口不请求；同会话消费者共享一次请求 / 一个 object URL；最多两个在途；
// 最后一个消费者离开、换会话、卸载均释放；迟到结果不回填；解码失败回收；
// 无缓存残留（再次进入视口重新读取）。
import { StrictMode } from "react";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import useArtifactImages from "@/mobile/useArtifactImages";
import { MockIntersectionObserver } from "./intersectionObserverMock";

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (v: T) => void;
  reject: (e: unknown) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function imageResponse(bytes = "png"): Response {
  return new Response(bytes, { status: 200, headers: { "content-type": "image/png" } });
}

let fetchMock: ReturnType<typeof vi.fn>;
let createObjectURL: ReturnType<typeof vi.fn>;
let revokeObjectURL: ReturnType<typeof vi.fn>;
let urlSeq = 0;

beforeEach(() => {
  MockIntersectionObserver.install();
  urlSeq = 0;
  createObjectURL = vi.fn(() => `blob:artifact-${++urlSeq}`);
  revokeObjectURL = vi.fn();
  Object.defineProperty(URL, "createObjectURL", {
    value: createObjectURL,
    configurable: true,
    writable: true,
  });
  Object.defineProperty(URL, "revokeObjectURL", {
    value: revokeObjectURL,
    configurable: true,
    writable: true,
  });
  fetchMock = vi.fn(async () => imageResponse());
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function mount(sessionId = "sess-1") {
  return renderHook(({ sid }: { sid: string }) => useArtifactImages(sid), {
    initialProps: { sid: sessionId },
  });
}

function fileCalls(): string[] {
  return fetchMock.mock.calls.map((c) => String(c[0])).filter((u) => u.includes("/m/api/v1/file?"));
}

describe("useArtifactImages：视口门与共享", () => {
  it("StrictMode 开发期「挂载→清理→再挂载」后仍能懒加载（dispose 只释放资源）", async () => {
    const { result } = renderHook(() => useArtifactImages("sess-strict"), {
      wrapper: StrictMode,
    });
    const el = document.createElement("div");
    act(() => {
      result.current.attach("/strict.png", el);
      MockIntersectionObserver.triggerAll(true);
    });
    await waitFor(() => expect(result.current.stateFor("/strict.png")?.status).toBe("ready"));
    expect(fileCalls().length).toBe(1);
  });

  it("元素未进入视口 → 不发任何请求", async () => {
    const { result } = mount();
    const el = document.createElement("div");
    act(() => {
      result.current.attach("/tmp/a.png", el);
    });
    expect(result.current.stateFor("/tmp/a.png")).toBeUndefined();
    expect(fetchMock).not.toHaveBeenCalled();
    expect(MockIntersectionObserver.instances.length).toBe(1);
    expect(MockIntersectionObserver.instances[0].elements.has(el)).toBe(true);
  });

  it("进入视口才读取；同一路径的两个消费者共享一次请求与同一个 object URL", async () => {
    const d = deferred<Response>();
    fetchMock.mockImplementation(async () => d.promise);
    const { result } = mount();
    const el1 = document.createElement("div");
    const el2 = document.createElement("div");
    let detach1 = () => {};
    let detach2 = () => {};
    act(() => {
      detach1 = result.current.attach("/tmp/a.png", el1);
      detach2 = result.current.attach("/tmp/a.png", el2);
      MockIntersectionObserver.triggerAll(true);
    });
    expect(fileCalls().length).toBe(1);
    await act(async () => {
      d.resolve(imageResponse());
      await d.promise;
    });
    await waitFor(() => expect(result.current.stateFor("/tmp/a.png")?.status).toBe("ready"));
    expect(result.current.stateFor("/tmp/a.png")?.url).toBe("blob:artifact-1");
    expect(createObjectURL).toHaveBeenCalledTimes(1);

    // 一个消费者离开：仍有一个消费者，URL 不得释放
    act(() => {
      detach1();
    });
    expect(revokeObjectURL).not.toHaveBeenCalled();
    expect(result.current.stateFor("/tmp/a.png")?.url).toBe("blob:artifact-1");

    // 最后一个消费者离开：立即释放，且不再保留状态（无消费者缓存）
    act(() => {
      detach2();
    });
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:artifact-1");
    expect(result.current.stateFor("/tmp/a.png")).toBeUndefined();
  });

  it("滚出视口（isIntersecting=false）：最后一个消费者释放资源；再次进入重新获取一次", async () => {
    const { result } = mount();
    const el = document.createElement("div");
    act(() => {
      result.current.attach("/exit.png", el);
      MockIntersectionObserver.triggerAll(true);
    });
    await waitFor(() => expect(result.current.stateFor("/exit.png")?.status).toBe("ready"));
    expect(fileCalls().length).toBe(1);

    // 滚出视口：最后一个消费者离开 → 立即 revoke 并删除资源
    act(() => {
      MockIntersectionObserver.triggerAll(false);
    });
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:artifact-1");
    expect(result.current.stateFor("/exit.png")).toBeUndefined();

    // 再次进入视口：重新 acquire 一次（离开视口不保留缓存）
    act(() => {
      MockIntersectionObserver.triggerAll(true);
    });
    await waitFor(() => expect(fileCalls().length).toBe(2));
    await waitFor(() => expect(result.current.stateFor("/exit.png")?.status).toBe("ready"));
    expect(result.current.stateFor("/exit.png")?.url).toBe("blob:artifact-2");
  });

  it("最多两个请求在途；完成一个才派发下一个", async () => {
    const pending: Array<Deferred<Response>> = [];
    fetchMock.mockImplementation(() => {
      const d = deferred<Response>();
      pending.push(d);
      return d.promise;
    });
    const { result } = mount();
    const els = ["/a.png", "/b.png", "/c.png"].map(() => document.createElement("div"));
    act(() => {
      result.current.attach("/a.png", els[0]);
      result.current.attach("/b.png", els[1]);
      result.current.attach("/c.png", els[2]);
      MockIntersectionObserver.triggerAll(true);
    });
    expect(fileCalls().length).toBe(2);
    await act(async () => {
      pending[0].resolve(imageResponse());
      await pending[0].promise;
    });
    await waitFor(() => expect(fileCalls().length).toBe(3));
    await act(async () => {
      pending[1].resolve(imageResponse());
      pending[2].resolve(imageResponse());
      await Promise.all([pending[1].promise, pending[2].promise]);
    });
    await waitFor(() => expect(result.current.stateFor("/c.png")?.status).toBe("ready"));
  });

  it("换会话释放旧 URL / 中止在途请求，旧图不串入新会话", async () => {
    const pending: Array<Deferred<Response>> = [];
    const signals: AbortSignal[] = [];
    fetchMock.mockImplementation((_url: RequestInfo | URL, init?: RequestInit) => {
      const d = deferred<Response>();
      pending.push(d);
      if (init?.signal) signals.push(init.signal);
      init?.signal?.addEventListener("abort", () =>
        d.reject(new DOMException("aborted", "AbortError"))
      );
      return d.promise;
    });
    const { result, rerender } = mount("sess-1");
    const el = document.createElement("div");
    act(() => {
      result.current.attach("/a.png", el);
      MockIntersectionObserver.triggerAll(true);
    });
    // 先完成一张，制造可回收 URL
    await act(async () => {
      pending[0].resolve(imageResponse());
      await pending[0].promise;
    });
    await waitFor(() => expect(result.current.stateFor("/a.png")?.status).toBe("ready"));
    const elB = document.createElement("div");
    act(() => {
      result.current.attach("/b.png", elB);
      MockIntersectionObserver.triggerAll(true);
    });
    expect(pending.length).toBe(2);

    rerender({ sid: "sess-2" });
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:artifact-1");
    expect(result.current.stateFor("/a.png")).toBeUndefined();
    expect(result.current.stateFor("/b.png")).toBeUndefined();
    // 旧会话在途请求被中止（迟到结果不得回填新会话）
    expect(signals.length).toBe(2);
    expect(signals[1].aborted).toBe(true);
  });

  it("卸载：释放已加载 URL；迟到结果不附着并立即回收", async () => {
    // 场景 A：已加载后卸载
    const { result, unmount } = mount();
    const el = document.createElement("div");
    act(() => {
      result.current.attach("/a.png", el);
      MockIntersectionObserver.triggerAll(true);
    });
    await waitFor(() => expect(result.current.stateFor("/a.png")?.status).toBe("ready"));
    unmount();
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:artifact-1");

    // 场景 B：请求在途时消费者离开，迟到结果直接回收、不发布
    const d = deferred<Response>();
    fetchMock.mockImplementation(async () => d.promise);
    const hook2 = mount("sess-3");
    const el2 = document.createElement("div");
    let detachLate = () => {};
    act(() => {
      detachLate = hook2.result.current.attach("/late.png", el2);
      MockIntersectionObserver.triggerAll(true);
    });
    expect(fileCalls().length).toBe(2);
    act(() => {
      detachLate();
    });
    await act(async () => {
      d.resolve(imageResponse());
      await d.promise;
    });
    await waitFor(() => expect(revokeObjectURL).toHaveBeenCalledWith("blob:artifact-2"));
    expect(hook2.result.current.stateFor("/late.png")).toBeUndefined();
  });

  it("无消费者缓存：最后一个离开后再次进入视口会重新读取", async () => {
    const { result } = mount();
    const el = document.createElement("div");
    let detach = () => {};
    act(() => {
      detach = result.current.attach("/a.png", el);
      MockIntersectionObserver.triggerAll(true);
    });
    await waitFor(() => expect(result.current.stateFor("/a.png")?.status).toBe("ready"));
    act(() => {
      detach();
    });
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:artifact-1");

    act(() => {
      detach = result.current.attach("/a.png", el);
      MockIntersectionObserver.triggerAll(true);
    });
    await waitFor(() => expect(fileCalls().length).toBe(2));
    expect(fileCalls().length).toBe(2);
  });

  it("解码失败：回收 URL，状态转错误且不自动重试", async () => {
    const { result } = mount();
    const el = document.createElement("div");
    act(() => {
      result.current.attach("/a.png", el);
      MockIntersectionObserver.triggerAll(true);
    });
    await waitFor(() => expect(result.current.stateFor("/a.png")?.status).toBe("ready"));
    act(() => {
      result.current.reportDecodeFailure("/a.png");
    });
    expect(result.current.stateFor("/a.png")?.status).toBe("error");
    expect(result.current.stateFor("/a.png")?.reason).toBe("decode_failed");
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:artifact-1");
    expect(fileCalls().length).toBe(1);
  });
});

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ApiError,
  deleteArchivedSession,
  fetchArchivedSessions,
  fetchFile,
  fetchHost,
  fetchSessionFiles,
  fetchSessions,
  pairWithPin,
  queueJump,
} from "@/mobile/api";

// setup.ts 的 msw server 会包一层全局 fetch；stub 覆盖其上，结束后还原防止泄漏到其他用例
afterEach(() => {
  vi.unstubAllGlobals();
});

describe("mobile api", () => {
  it("fetchSessionFiles 兼容旧载荷并透传新 fileKey", async () => {
    const legacy = {
      path: "old.txt",
      lastSeq: 1,
      lastTs: null,
      hits: 1,
      modified: false,
      origin: "tool_read",
    };
    const keyed = { ...legacy, path: "new.txt", fileKey: "/project/new.txt" };
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ files: [legacy, keyed], truncated: false })))
    );
    const result = await fetchSessionFiles("codex", "session", 200);
    expect(result.files[0].path).toBe("old.txt");
    expect(result.files[0].fileKey).toBeUndefined();
    expect(result.files[1].fileKey).toBe("/project/new.txt");
    const previewFetch = vi.fn(
      async () =>
        new Response(JSON.stringify({ content: "old", mime: "text/plain" }), {
          headers: { "content-type": "application/json" },
        })
    );
    vi.stubGlobal("fetch", previewFetch);
    expect(await fetchFile("session", result.files[0].path)).toEqual({
      kind: "text",
      content: "old",
      mime: "text/plain",
    });
    expect(previewFetch).toHaveBeenCalledWith("/m/api/v1/file?session_id=session&path=old.txt");
  });

  it("pairWithPin POST /pair/pin 携带 pin，成功透传 ok", async () => {
    const f = vi.fn(async () => new Response('{"ok":true}', { status: 200 }));
    vi.stubGlobal("fetch", f);
    expect((await pairWithPin("4827")).ok).toBe(true);
    expect(f).toHaveBeenCalledWith(
      "/m/api/v1/pair/pin",
      expect.objectContaining({
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ pin: "4827" }),
      })
    );
  });

  it("pairWithPin 错误体 JSON 挂到 ApiError.data（error/remaining/retryAfter 分診依据）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"invalid_pin","remaining":2}', { status: 401 }))
    );
    const err = await pairWithPin("0000").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(401);
    expect((err as ApiError).data).toEqual({ error: "invalid_pin", remaining: 2 });
  });

  it("pairWithPin 网络异常 → ApiError status=null（PairPage 网络文案分診依据）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      })
    );
    const err = await pairWithPin("4827").catch((e: unknown) => e);
    expect((err as ApiError).status).toBeNull();
  });

  it("403 返回 null 触发回配对页", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("", { status: 403 }))
    );
    expect(await fetchSessions()).toBeNull();
  });

  it("fetchHost GET /m/api/v1/host，透传载荷；403 同样返回 null（设备失效语义一致）", async () => {
    const host = {
      host: { name: "JARVIS-Win", platform: "windows", version: "0.4.1" },
      enabledTools: ["claude"],
    };
    const f = vi.fn(async () => new Response(JSON.stringify(host), { status: 200 }));
    vi.stubGlobal("fetch", f);
    expect(await fetchHost<typeof host>()).toEqual(host);
    expect(f).toHaveBeenCalledWith("/m/api/v1/host");

    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("", { status: 403 }))
    );
    expect(await fetchHost()).toBeNull();
  });

  it("queueJump 200 queued 回执透传（F7④：守卫忙 / Deferred / Suspended 排队语义，与后端契约对齐）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ status: "queued", itemId: 7, position: 2 }), {
            status: 200,
          })
      )
    );
    const r = await queueJump("sess-1", 7);
    // queued 变体必须携带 itemId/position（类型层契约）；运行期透传校验
    if (r.status === "queued") {
      expect(r.itemId).toBe(7);
      expect(r.position).toBe(2);
    } else {
      throw new Error("queued 变体丢失：回执未按排队语义透传");
    }
  });

  it("fetchArchivedSessions 携带 days 且解析载荷", async () => {
    const f = vi.fn(
      async () =>
        new Response(JSON.stringify({ archived: [], projects: ["p1"] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        })
    );
    vi.stubGlobal("fetch", f);
    const p = await fetchArchivedSessions(7);
    expect(f).toHaveBeenCalledWith("/m/api/v1/sessions-archived?days=7");
    expect(p?.projects).toEqual(["p1"]);
  });

  it("fetchArchivedSessions 403 → null（回配对页口径与 fetchSessions 一致）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("", { status: 403 }))
    );
    expect(await fetchArchivedSessions(1)).toBeNull();
  });

  it("deleteArchivedSession 带 id 走单条、缺省走 all=1，返回删除计数", async () => {
    const f = vi.fn(async () => new Response('{"ok":true,"deleted":2}', { status: 200 }));
    vi.stubGlobal("fetch", f);
    expect(await deleteArchivedSession("sess-1")).toBe(2);
    expect(f).toHaveBeenLastCalledWith(
      "/m/api/v1/sessions-archived?session_id=sess-1",
      expect.objectContaining({ method: "DELETE" })
    );
    await deleteArchivedSession();
    expect(f).toHaveBeenLastCalledWith(
      "/m/api/v1/sessions-archived?all=1",
      expect.objectContaining({ method: "DELETE" })
    );
  });
});

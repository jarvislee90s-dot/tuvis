import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, createSession, fetchCreateProjects, fetchCreateStatus } from "@/mobile/api";

// setup.ts 的 msw server 会包一层全局 fetch；stub 覆盖其上，结束后还原防止泄漏到其他用例
afterEach(() => {
  vi.unstubAllGlobals();
});

/** 取 mock fetch 第 n 次调用的请求体 JSON（形态断言用） */
function bodyOf(f: ReturnType<typeof vi.fn>, n = 0): Record<string, unknown> {
  const init = f.mock.calls[n]?.[1] as RequestInit | undefined;
  return JSON.parse(String(init?.body)) as Record<string, unknown>;
}

describe("fetchCreateProjects（C10 新建会话目标目录候选）", () => {
  it("默认 days=7，解析 projects 载荷（camelCase 四字段逐项断言）", async () => {
    const payload = {
      projects: [
        {
          path: "E:\\proj\\alpha",
          lastActiveAt: "2026-10-02T03:00:00Z",
          tools: ["claude", "codex"],
          activeTools: ["claude"],
        },
      ],
    };
    const f = vi.fn(async () => new Response(JSON.stringify(payload), { status: 200 }));
    vi.stubGlobal("fetch", f);
    const p = await fetchCreateProjects();
    expect(f).toHaveBeenCalledWith("/m/api/v1/create-projects?days=7");
    expect(p?.projects).toHaveLength(1);
    expect(p?.projects[0].path).toBe("E:\\proj\\alpha");
    expect(p?.projects[0].lastActiveAt).toBe("2026-10-02T03:00:00Z");
    expect(p?.projects[0].tools).toEqual(["claude", "codex"]);
    expect(p?.projects[0].activeTools).toEqual(["claude"]);
  });

  it("自定义 days 透传查询串", async () => {
    const f = vi.fn(async () => new Response('{"projects":[]}', { status: 200 }));
    vi.stubGlobal("fetch", f);
    const p = await fetchCreateProjects(30);
    expect(f).toHaveBeenCalledWith("/m/api/v1/create-projects?days=30");
    expect(p?.projects).toEqual([]);
  });

  it("403 → null（设备失效回配对页，fetchArchivedSessions 同口径）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("", { status: 403 }))
    );
    expect(await fetchCreateProjects()).toBeNull();
  });

  it("500 → 抛 ApiError(500)（非 403 失败不静默）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"internal"}', { status: 500 }))
    );
    const err = await fetchCreateProjects().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(500);
  });

  it("网络异常 → 抛 ApiError(status=null)", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      })
    );
    const err = await fetchCreateProjects().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBeNull();
  });
});

describe("createSession（C10 新建会话投递）", () => {
  it("成功 200 透传 taskId/hasActiveSession，请求体 camelCase 逐键", async () => {
    const f = vi.fn(
      async () => new Response('{"taskId":7,"hasActiveSession":true}', { status: 200 })
    );
    vi.stubGlobal("fetch", f);
    const r = await createSession({
      tool: "claude",
      projectPath: "E:\\proj\\alpha",
      firstMessage: "hi",
    });
    expect(r).toEqual({ taskId: 7, hasActiveSession: true });
    expect(f).toHaveBeenCalledWith(
      "/m/api/v1/session-create",
      expect.objectContaining({
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          tool: "claude",
          projectPath: "E:\\proj\\alpha",
          firstMessage: "hi",
        }),
      })
    );
  });

  it("firstMessage 缺省 → 请求体不含该键（后端缺省探针 hi）", async () => {
    const f = vi.fn(
      async () => new Response('{"taskId":1,"hasActiveSession":false}', { status: 200 })
    );
    vi.stubGlobal("fetch", f);
    await createSession({ tool: "codex", projectPath: "/tmp/p" });
    const body = bodyOf(f);
    expect(body).toEqual({ tool: "codex", projectPath: "/tmp/p" });
    expect("firstMessage" in body).toBe(false);
  });

  it("400 + reasonCode → {kind:'bad_request', reasonCode}（工具门/路径码/建目录失败分診）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response('{"error":"bad_request","reasonCode":"tool_unavailable"}', {
            status: 400,
          })
      )
    );
    expect(await createSession({ tool: "claude", projectPath: "/tmp/p" })).toEqual({
      kind: "bad_request",
      reasonCode: "tool_unavailable",
    });
  });

  it("400 无 reasonCode（超长入参等裸 bad_request）→ {kind:'bad_request'} 且 reasonCode 缺省", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"bad_request"}', { status: 400 }))
    );
    const r = await createSession({ tool: "claude", projectPath: "/tmp/p" });
    expect(r).toEqual({ kind: "bad_request" });
    expect((r as { reasonCode?: string }).reasonCode).toBeUndefined();
  });

  it("409 → {kind:'conflict'}（全局单飞占用，不抛异常）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"conflict"}', { status: 409 }))
    );
    expect(await createSession({ tool: "claude", projectPath: "/tmp/p" })).toEqual({
      kind: "conflict",
    });
  });

  it("409 OC_LEGACY_DISABLED → {kind:'conflict', reasonCode, reason}（策略门拒绝分診，H14 O1）", async () => {
    // 实测形态（H14 §3.2）：reason 带「OC_LEGACY_DISABLED: 」机器前缀——上屏分診在
    // 面板层按码走 i18n，api 层只负责把字段带出（与 400 同构）
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              error: "OC_LEGACY_DISABLED",
              reasonCode: "OC_LEGACY_DISABLED",
              reason: "OC_LEGACY_DISABLED: OpenCode 当前仅允许原生交互，旧终端交互已停用",
            }),
            { status: 409 }
          )
      )
    );
    const r = await createSession({ tool: "opencode", projectPath: "/tmp/p" });
    expect(r).toEqual({
      kind: "conflict",
      reasonCode: "OC_LEGACY_DISABLED",
      reason: "OC_LEGACY_DISABLED: OpenCode 当前仅允许原生交互，旧终端交互已停用",
    });
  });

  it("403 → 抛 ApiError(403)（设备失效，非 400/409 走既有 POST 异常惯例）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"forbidden"}', { status: 403 }))
    );
    const err = await createSession({ tool: "claude", projectPath: "/tmp/p" }).catch(
      (e: unknown) => e
    );
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(403);
    expect((err as ApiError).data).toEqual({ error: "forbidden" });
  });

  it("500 → 抛 ApiError(500)（错误体解析进 data）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"internal"}', { status: 500 }))
    );
    const err = await createSession({ tool: "claude", projectPath: "/tmp/p" }).catch(
      (e: unknown) => e
    );
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(500);
  });

  it("网络异常 → 抛 ApiError(status=null)", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      })
    );
    const err = await createSession({ tool: "claude", projectPath: "/tmp/p" }).catch(
      (e: unknown) => e
    );
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBeNull();
  });
});

describe("fetchCreateStatus（C10 新建任务进度轮询）", () => {
  it("200 透传四字段（运行中：detail 有值、sessionId/spawnedPid 未定）", async () => {
    const f = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            phase: "injecting_first",
            detail: "已注入首句，等待物化",
            sessionId: null,
            spawnedPid: 4242,
          }),
          { status: 200 }
        )
    );
    vi.stubGlobal("fetch", f);
    const s = await fetchCreateStatus(7);
    expect(f).toHaveBeenCalledWith("/m/api/v1/session-create/status?taskId=7");
    expect(s).toEqual({
      phase: "injecting_first",
      detail: "已注入首句，等待物化",
      sessionId: null,
      spawnedPid: 4242,
    });
  });

  it("200 done 终态：sessionId 落地", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              phase: "done",
              detail: null,
              sessionId: "sess-9",
              spawnedPid: 4242,
            }),
            { status: 200 }
          )
      )
    );
    const s = await fetchCreateStatus(1);
    expect(s).toEqual({ phase: "done", detail: null, sessionId: "sess-9", spawnedPid: 4242 });
  });

  it("404 → {kind:'no_task'}（任务失效/主机重启，引导重试）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"no_task"}', { status: 404 }))
    );
    expect(await fetchCreateStatus(999)).toEqual({ kind: "no_task" });
  });

  it("500 → 抛 ApiError(500)", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"internal"}', { status: 500 }))
    );
    const err = await fetchCreateStatus(7).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(500);
  });

  it("网络异常 → 抛 ApiError(status=null)", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      })
    );
    const err = await fetchCreateStatus(7).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBeNull();
  });
});

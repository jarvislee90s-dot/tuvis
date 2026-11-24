/**
 * N1 逐路由访问门禁测试（RED 先行）——镜像 Mac 补探测报告 §11.3 的单变量矩阵。
 *
 * 事实依据（§3.5）：自注册 `webServer` 路由**不继承**官方 `/api` 认证与
 * Host/Origin 围栏——伪装的 Host/Origin 照旧 200。故插件必须自己给每条路由
 * 定义访问边界。官方围栏语义（§4.4/§11.3 定案）：
 *   A. `Host` 只要解析为回环字面量即可过闸（127.0.0.0/8、`::1`/`[::1]`、
 *      `localhost`），**端口不参与判定**；非回环 → 403；
 *   B. `Origin` **存在时**其 authority 必须与 `Host` **逐字相同**（含端口），
 *      任何错配 → 403；`Origin` 缺失 → 放行；`Origin: null` → 403；
 *   C. `sec-fetch-site: cross-site` → 403；
 *   D. 代理头不参与判定（`X-Forwarded-For` / `X-Forwarded-Host` 不改变结果）。
 *
 * 观测面 = 路由 handler 对假 req/res 写出的状态码与响应体（不启真实监听）。
 */

import { describe, expect, it } from "vitest";
import type { IncomingMessage, ServerResponse } from "node:http";
import { apply as builtApply, type WebRoute, type HostContext } from "../lib/index.js";

/** 录制型假响应（只记 status/headers/body；不透出真实套接字）。 */
function fakeResponse() {
  const state = { status: 0, headers: {} as Record<string, string>, body: "" };
  const res = {
    writeHead(status: number, headers?: Record<string, string>) {
      state.status = status;
      Object.assign(state.headers, headers ?? {});
      return res;
    },
    end(body?: string) {
      state.body = body ?? "";
      return res;
    },
  };
  return { state, res: res as unknown as ServerResponse };
}

/** 构造带指定头的最小请求（node 的 req.headers 已小写化）。 */
function fakeRequest(headers: Record<string, string | undefined>): IncomingMessage {
  const lowered: Record<string, string> = {};
  for (const [key, value] of Object.entries(headers)) {
    if (value !== undefined) lowered[key.toLowerCase()] = value;
  }
  return { headers: lowered, method: "GET", url: "/" } as unknown as IncomingMessage;
}

/** 挂载插件并取回指定路径的路由 handler（假 webServer 记录注册）。 */
function routeHandler(path: string): (req: IncomingMessage, res: ServerResponse) => unknown {
  const routes = new Map<string, WebRoute>();
  const webServer = {
    register(route: WebRoute): () => void {
      routes.set(route.path, route);
      return () => routes.delete(route.path);
    },
  };
  const ctx = { webServer } as unknown as HostContext;
  const dispose = builtApply(ctx);
  const route = routes.get(path);
  if (route === undefined) {
    dispose();
    throw new Error(`路由未注册：${path}（已注册：${[...routes.keys()].join(", ")}）`);
  }
  return (req, res) => route.handler(req, res);
}

async function call(path: string, headers: Record<string, string | undefined>): Promise<{
  status: number;
  body: string;
  headers: Record<string, string>;
}> {
  const handler = routeHandler(path);
  const { state, res } = fakeResponse();
  await handler(fakeRequest(headers), res);
  return { status: state.status, body: state.body, headers: state.headers };
}

const HEALTH = "/tuvis-remote/health";
const BINDING_STATE = "/tuvis-remote/binding-state";

/** 报告 §11.3 A 组：不带 Origin，只改 Host（回环字面量全部放行）。 */
const LOOPBACK_HOSTS = [
  "127.0.0.1:51583",
  "127.0.0.2:51583",
  "127.255.255.255:51583",
  "127.0.0.1",
  "localhost:51583",
  "localhost",
  "[::1]:51583",
];

/** 报告 §11.3 A 组：非回环（必须 403）。 */
const NON_LOOPBACK_HOSTS = ["example.com", "example.com:51583", "10.0.0.5:51583", "0.0.0.0:51583"];

describe("N1 路由门禁（镜像报告 §11.3 矩阵）", () => {
  it("loopback_host_literals_pass_on_every_route", async () => {
    for (const path of [HEALTH, BINDING_STATE]) {
      for (const host of LOOPBACK_HOSTS) {
        const { status } = await call(path, { host });
        expect(status, `${path} Host=${host} 应放行`).toBe(200);
      }
    }
  });

  it("non_loopback_host_is_rejected_on_every_route", async () => {
    for (const path of [HEALTH, BINDING_STATE]) {
      for (const host of NON_LOOPBACK_HOSTS) {
        const { status, body } = await call(path, { host });
        expect(status, `${path} Host=${host} 应 403`).toBe(403);
        // 不回声攻击者输入（响应体不得含其 Host 值）
        expect(body).not.toContain(host);
      }
    }
  });

  it("origin_mismatch_is_rejected_and_same_authority_passes", async () => {
    // 同期 Origin（逐字相同，含端口）→ 放行
    for (const [host, origin] of [
      ["127.0.0.2:51583", "http://127.0.0.2:51583"],
      ["localhost:51583", "http://localhost:51583"],
      ["127.0.0.1", "http://127.0.0.1"],
    ] as const) {
      const { status } = await call(BINDING_STATE, { host, origin });
      expect(status, `Host=${host} Origin=${origin} 应放行`).toBe(200);
    }

    // 错配（§11.3 C/D 组）→ 403
    for (const [host, origin] of [
      ["127.0.0.2:51583", "http://127.0.0.1:51583"],
      ["localhost:51583", "http://127.0.0.1:51583"],
      ["127.0.0.1:9999", "http://127.0.0.1:51583"],
      ["example.com:51583", "http://example.com:51583"],
      ["127.0.0.1:51583", "null"],
      ["127.0.0.1:51583", "http://evil.example"],
    ] as const) {
      const { status } = await call(BINDING_STATE, { host, origin });
      expect(status, `Host=${host} Origin=${origin} 应 403`).toBe(403);
    }
  });

  it("absent_origin_passes_and_cross_site_fetch_is_rejected", async () => {
    // 顶层导航不带 Origin → 只受 Host 回环约束
    expect((await call(BINDING_STATE, { host: "127.0.0.1:51583" })).status).toBe(200);

    for (const secFetchSite of ["cross-site", "Cross-Site", "same-site", "none"]) {
      const { status } = await call(HEALTH, {
        host: "127.0.0.1:51583",
        "sec-fetch-site": secFetchSite,
      });
      // 只拒 cross-site；其余同站/直连形态不额外拦（避免误伤 Desktop 窗口）
      expect(status, `sec-fetch-site=${secFetchSite}`).toBe(
        secFetchSite.toLowerCase() === "cross-site" ? 403 : 200,
      );
    }

    // cross-site 在每条路由上都拒
    expect(
      (await call(BINDING_STATE, { host: "127.0.0.1:51583", "sec-fetch-site": "cross-site" })).status,
    ).toBe(403);
  });

  it("proxy_headers_do_not_change_the_verdict", async () => {
    // §11.3 D：XFF/XFH 不参与判定（官方行为；插件同样照此）
    const base = await call(BINDING_STATE, { host: "127.0.0.1:51583" });
    const withProxy = await call(BINDING_STATE, {
      host: "127.0.0.1:51583",
      "x-forwarded-for": "203.0.113.7",
      "x-forwarded-host": "example.com",
    });
    expect(withProxy.status).toBe(base.status);

    const nonLoop = await call(BINDING_STATE, {
      host: "example.com",
      "x-forwarded-for": "127.0.0.1",
    });
    expect(nonLoop.status).toBe(403);
  });

  it("health_body_is_minimal_and_free_of_host_internals", async () => {
    const { status, body, headers } = await call(HEALTH, { host: "127.0.0.1:51583" });
    expect(status).toBe(200);
    expect(headers["content-type"]).toContain("application/json");

    const parsed = JSON.parse(body) as Record<string, unknown>;
    // 只留存活证明：插件名 + 协议代际（无主机名/端口/路径/进程信息）
    expect(Object.keys(parsed).sort()).toEqual(["bindingProtocol", "plugin"]);
    expect(body).not.toContain("127.0.0.1");
    expect(body).not.toContain("/Users/");
  });

  it("binding_state_is_read_only_and_exposes_no_credential_material", async () => {
    const { status, body } = await call(BINDING_STATE, { host: "127.0.0.1:51583" });
    expect(status).toBe(200);
    const parsed = JSON.parse(body) as Record<string, unknown>;
    expect(Object.keys(parsed).sort()).toEqual(["bindingId", "displayCode", "state"]);
    // 除 GET 之外没有任何写/授权入口：插件路由表里只有这两条只读路由
    expect(body.toLowerCase()).not.toContain("cookie");
    expect(body.toLowerCase()).not.toContain("token");
  });

  it("only_two_named_routes_exist_and_both_are_fenced", async () => {
    const registered: string[] = [];
    const webServer = {
      register(route: WebRoute): () => void {
        registered.push(`${route.kind} ${route.path}`);
        return () => {};
      },
    };
    const dispose = builtApply({ webServer } as unknown as HostContext);
    expect(registered.sort()).toEqual([`exact ${BINDING_STATE}`, `exact ${HEALTH}`]);
    // 没有任何授权写入路由（批准只在兔维斯 Tauri IPC 侧）
    for (const route of registered) {
      expect(route).not.toMatch(/approve|grant|authorize|revoke|start|bind$/i);
    }
    dispose();
  });
});

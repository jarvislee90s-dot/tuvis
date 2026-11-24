/**
 * 真实 Cookie 兑换测试（RED 先行）——按报告 §4.1/§4.2 的实测形状实现。
 *
 * 契约（实测读数）：
 *  1. `ctx.connection.authenticatedUrl(<绝对 URL>)`，arity=1；入参必须是可
 *     `new URL()` 解析的**绝对** URL（相对形态全 TypeError）；返回
 *     `http://127.0.0.1:<port>/?token=<43 字符 base64url>`。
 *  2. `GET <origin>/connect?token=<T>&interactions=answer` →
 *     **302** + `set-cookie: dsh-auth-<b64url(sha256(authority))>=<231 字符>;
 *     HttpOnly; SameSite=Strict`。重定向必须手动（`redirect: "manual"`），
 *     绝不自动跟随。
 *  3. 任何偏离（非 302 / 无 set-cookie）→ 带**阶段名**的类型化错误；
 *     错误消息与任何调试输出**不含 token、不含 cookie 值**。
 *
 * 观测面 = 假 fetch 收到的 URL/选项 + 兑换返回的 cookie 串 + 抛出错误的形状。
 */

import { describe, expect, it } from "vitest";
import { CookieExchangeError, createBrowserCookieExchange, createHostExchange } from "../src/index.js";

interface FakeCall {
  url: string;
  init: RequestInit | undefined;
}

/** 报告 §4.2 的实测 cookie 形态：`name=value` 全串（231 字符值）。 */
const COOKIE_NAME = "dsh-auth-nHw1fM5SwHWbLi7Bxz5TD_BmH9g9dxktNHUW4h2yhZo";
const COOKIE_VALUE = `v2.${"A".repeat(120)}.${"B".repeat(64)}`;
const SET_COOKIE = `${COOKIE_NAME}=${COOKIE_VALUE}; Max-Age=2592000; Path=/; HttpOnly; SameSite=Strict`;

const TOKEN = "T".repeat(43);

/** 假宿主：`webServer`（protocol/port）+ `connection.authenticatedUrl`。 */
function fakeHost(options: { omitAuthenticatedUrl?: boolean } = {}) {
  const urlCalls: string[] = [];
  const webServer = { protocol: "http:", port: 51583 };
  const connection = options.omitAuthenticatedUrl
    ? {}
    : {
        authenticatedUrl(input: string): string {
          urlCalls.push(input);
          return `${input}/?token=${TOKEN}`;
        },
      };
  return { webServer, connection, urlCalls };
}

/** 假 fetch：按脚本返回响应（可断言 init 选项）。 */
function fakeFetch(
  respond: (url: string) => Response | Promise<Response>,
): { fetch: typeof fetch; calls: FakeCall[] } {
  const calls: FakeCall[] = [];
  const impl = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : String(input);
    calls.push({ url, init });
    return await respond(url);
  }) as unknown as typeof fetch;
  return { fetch: impl, calls };
}

function response302WithCookie(): Response {
  // 用真实 Headers 承载 set-cookie（undici 的 Headers 支持 getSetCookie 语义）
  const headers = new Headers();
  headers.append("set-cookie", SET_COOKIE);
  return new Response(null, { status: 302, headers });
}

describe("createHostExchange（真实 /connect 形状）", () => {
  it("exchange_calls_authenticatedUrl_with_an_absolute_origin_and_reads_302_cookie", async () => {
    const host = fakeHost();
    const net = fakeFetch(() => response302WithCookie());
    const exchange = createHostExchange(host.connection, host.webServer, net.fetch);

    const cookie = await exchange.exchange();

    // ① authenticatedUrl 收到绝对 origin（官方 Desktop 宿主同款拼法）
    expect(host.urlCalls).toEqual(["http://127.0.0.1:51583"]);

    // ② /connect 带 token 与 interactions=answer，且强制 manual redirect
    expect(net.calls).toHaveLength(1);
    const call = net.calls[0]!;
    expect(call.url).toBe(`http://127.0.0.1:51583/connect?token=${TOKEN}&interactions=answer`);
    expect(call.init?.redirect).toBe("manual");
    expect(call.init?.method ?? "GET").toBe("GET");

    // ③ 返回完整 name=value（不含属性）
    expect(cookie).toBe(`${COOKIE_NAME}=${COOKIE_VALUE}`);
  });

  it("exchange_rejects_a_followed_redirect_and_non_302_statuses", async () => {
    const host = fakeHost();
    // 200（跟随后的页面）/ 303（浏览器路线）/ 401 / 400 都不是本路径的成功形态
    for (const status of [200, 303, 401, 400]) {
      const net = fakeFetch(
        () => new Response("nope", { status, headers: { "set-cookie": SET_COOKIE } }),
      );
      const exchange = createHostExchange(host.connection, host.webServer, net.fetch);
      await expect(exchange.exchange()).rejects.toMatchObject({
        name: "CookieExchangeError",
        stage: "connect-status",
      });
    }
  });

  it("exchange_rejects_a_302_without_the_auth_cookie", async () => {
    const host = fakeHost();
    const net = fakeFetch(() => new Response(null, { status: 302 }));
    const exchange = createHostExchange(host.connection, host.webServer, net.fetch);

    await expect(exchange.exchange()).rejects.toMatchObject({
      name: "CookieExchangeError",
      stage: "set-cookie",
    });
  });

  it("exchange_reports_the_missing_authenticatedUrl_stage_explicitly", async () => {
    const host = fakeHost({ omitAuthenticatedUrl: true });
    const net = fakeFetch(() => response302WithCookie());
    const exchange = createHostExchange(host.connection, host.webServer, net.fetch);

    await expect(exchange.exchange()).rejects.toMatchObject({
      name: "CookieExchangeError",
      stage: "authenticated-url",
    });
  });

  it("exchange_reports_a_non_url_result_from_authenticatedUrl", async () => {
    const webServer = { protocol: "http:", port: 51583 };
    const connection = { authenticatedUrl: () => "/relative?token=x" };
    const net = fakeFetch(() => response302WithCookie());
    const exchange = createHostExchange(connection, webServer, net.fetch);

    await expect(exchange.exchange()).rejects.toMatchObject({
      name: "CookieExchangeError",
      stage: "authenticated-url",
    });
  });

  it("exchange_wraps_transport_failures_with_a_stage_and_no_secret", async () => {
    const host = fakeHost();
    const net = fakeFetch(() => {
      throw new Error("ECONNREFUSED");
    });
    const exchange = createHostExchange(host.connection, host.webServer, net.fetch);

    await expect(exchange.exchange()).rejects.toMatchObject({
      name: "CookieExchangeError",
      stage: "connect-request",
    });
  });

  it("neither_token_nor_cookie_value_appears_in_errors_or_debug_output", async () => {
    const host = fakeHost();
    // 让每个阶段都失败一次，收集所有错误消息
    const failures = [
      fakeFetch(() => new Response("bad", { status: 401 })),
      fakeFetch(() => new Response(null, { status: 302 })),
      fakeFetch(() => {
        throw new Error(`connect failed for token ${TOKEN}`);
      }),
    ];
    const messages: string[] = [];
    for (const net of failures) {
      const exchange = createHostExchange(host.connection, host.webServer, net.fetch);
      try {
        await exchange.exchange();
      } catch (error) {
        const typed = error as CookieExchangeError;
        // 阶段 + 类型都在，秘密都不在
        expect(typed.name).toBe("CookieExchangeError");
        expect(typeof typed.stage).toBe("string");
        messages.push(typed.message, JSON.stringify(typed), String(typed.stack ?? ""));
      }
    }
    expect(messages.length).toBeGreaterThan(0);
    for (const message of messages) {
      expect(message).not.toContain(TOKEN);
      expect(message).not.toContain(COOKIE_VALUE);
    }

    // 成功路径的返回值只含 name=value，且不写任何日志
    const logged: unknown[] = [];
    const originalLog = console.log;
    const originalError = console.error;
    console.log = (...args: unknown[]) => logged.push(args);
    console.error = (...args: unknown[]) => logged.push(args);
    try {
      const net = fakeFetch(() => response302WithCookie());
      const exchange = createHostExchange(host.connection, host.webServer, net.fetch);
      await exchange.exchange();
    } finally {
      console.log = originalLog;
      console.error = originalError;
    }
    expect(logged).toEqual([]);
  });
});

describe("createBrowserCookieExchange（P1-2a：首页要浏览器会话凭据）", () => {
  // 可抓住的缺陷：把 `/connect` 的 API operator 凭据（payload version 2）当首页
  // 凭据用。官方 `authorizeIndex` 只认 version 1 的浏览器 cookie，拿错受众的结果
  // 是首页恒未授权——控制台会得到一个打不开的页面。
  it("exchanges_the_launch_token_for_a_browser_cookie_via_303_on_the_root_path", async () => {
    const host = fakeHost();
    const net = fakeFetch(() => {
      const headers = new Headers();
      headers.append("set-cookie", SET_COOKIE);
      return new Response(null, { status: 303, headers });
    });
    const exchange = createBrowserCookieExchange(host.connection, host.webServer, net.fetch);

    const cookie = await exchange();
    expect(cookie).toBe(`${COOKIE_NAME}=${COOKIE_VALUE}`);

    // 路径必须是 `/`（authorizeIndex 的路由），不是 `/connect`。
    expect(net.calls).toHaveLength(1);
    expect(new URL(net.calls[0]!.url).pathname).toBe("/");
    expect(new URL(net.calls[0]!.url).searchParams.get("token")).toBe(TOKEN);
    // 必须禁止自动跟随：跟随会把 303 变成 200 页面并丢掉 set-cookie 原形。
    expect(net.calls[0]!.init?.redirect).toBe("manual");
  });

  // 反面：`/connect` 那条 302 的凭据**不得**被当首页凭据返回。
  it("returns_null_instead_of_accepting_the_302_api_credential_shape", async () => {
    const host = fakeHost();
    const net = fakeFetch(() => response302WithCookie());
    const exchange = createBrowserCookieExchange(host.connection, host.webServer, net.fetch);
    expect(await exchange()).toBeNull();
  });

  // 不可兑换时返回 null（不抛）：首页通道要能如实标「不可用」，而不是拖垮绑定。
  it("returns_null_when_the_host_exposes_no_authenticated_url", async () => {
    const host = fakeHost({ omitAuthenticatedUrl: true });
    const net = fakeFetch(() => response302WithCookie());
    const exchange = createBrowserCookieExchange(host.connection, host.webServer, net.fetch);
    expect(await exchange()).toBeNull();
    expect(net.calls).toHaveLength(0);
  });
});

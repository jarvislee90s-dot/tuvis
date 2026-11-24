// 仅验证测试夹具的隔离与计数契约；Node HTTP 客户端不构成浏览器隔离证据。
import assert from "node:assert/strict";
import { test } from "node:test";
import { request as httpRequest } from "node:http";
import { Script } from "node:vm";
import { startFixture } from "./mam-device-cookie.fixture.mjs";

async function fixture(t) {
  const app = await startFixture();
  t.after(() => app.close());
  return app;
}

async function pair(app) {
  const response = await fetch(`${app.origins.main}/m/fixture/pair`, {
    method: "POST",
  });
  assert.equal(response.status, 200);
  return response.headers.get("set-cookie");
}

test("临时配对下发现库 host-only HttpOnly Lax Cookie", async (t) => {
  const app = await fixture(t);
  const cookie = await pair(app);
  assert.match(
    cookie,
    /^mam_device=[0-9a-f]{32}; Path=\/m; HttpOnly; SameSite=Lax; Max-Age=15552000$/
  );
});

test("无 Cookie 的简单 POST 在假管理计数器之前被拒绝", async (t) => {
  const app = await fixture(t);
  const response = await fetch(`${app.origins.main}/m/api/v1/probe/no-cookie`, {
    method: "POST",
    headers: { "content-type": "text/plain" },
    body: "fixture-only",
  });
  assert.equal(response.status, 403);
  assert.deepEqual(app.evidence().counts, { attempted: 1, authorized: 0, rejected: 1 });
});

test("有效临时 Cookie 可命中假管理处理器且日志不泄露 Cookie 值", async (t) => {
  const app = await fixture(t);
  const cookie = (await pair(app)).split(";")[0];
  const response = await fetch(`${app.origins.main}/m/api/v1/probe/positive`, {
    headers: { cookie },
  });
  assert.equal(response.status, 200);
  assert.equal(app.evidence().counts.authorized, 1);
  assert.equal(JSON.stringify(app.evidence()).includes(cookie), false);
});

test("携有效 Cookie 的跨 origin 简单 POST 不能被夹具凭空加上的 Origin gate 拦截", async (t) => {
  const app = await fixture(t);
  const cookie = (await pair(app)).split(";")[0];
  const response = await fetch(`${app.origins.main}/m/api/v1/probe/port-counterexample`, {
    method: "POST",
    headers: { cookie, origin: app.origins.sameHostConsole, "content-type": "text/plain" },
    body: "fixture-only",
  });
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("access-control-allow-origin"), null);
  assert.equal(app.evidence().counts.authorized, 1);
});

test("预检自身未授权且不设置 CORS 放行头", async (t) => {
  const app = await fixture(t);
  const response = await fetch(`${app.origins.main}/m/api/v1/probe/preflight`, {
    method: "OPTIONS",
    headers: {
      origin: app.origins.sameHostConsole,
      "access-control-request-method": "POST",
      "access-control-request-headers": "content-type",
    },
  });
  assert.equal(response.status, 403);
  assert.equal(response.headers.get("access-control-allow-origin"), null);
  assert.equal(app.evidence().counts.authorized, 0);
});

async function upgrade(app, cookie) {
  return new Promise((resolve, reject) => {
    const headers = {
      connection: "Upgrade",
      upgrade: "websocket",
      "sec-websocket-key": "Zml4dHVyZS1vbmx5LWtleQ==",
      "sec-websocket-version": "13",
      origin: app.origins.sameHostConsole,
      ...(cookie ? { cookie } : {}),
    };
    const req = httpRequest(`${app.origins.main}/m/api/v1/probe/ws`, { headers });
    req.on("upgrade", (response, socket) => {
      socket.destroy();
      resolve(response.statusCode);
    });
    req.on("response", (response) => {
      response.resume();
      resolve(response.statusCode);
    });
    req.on("error", reject);
    req.setTimeout(3000, () => req.destroy(new Error("fixture WebSocket handshake timeout")));
    req.end();
  });
}

test("跨 origin WS 携有效临时 Cookie 时记录 101 授权反例", async (t) => {
  const app = await fixture(t);
  const cookie = (await pair(app)).split(";")[0];
  assert.equal(await upgrade(app, cookie), 101);
  assert.equal(app.evidence().attempts[0].transport, "ws");
  assert.equal(app.evidence().counts.authorized, 1);
});

test("无 Cookie 的 WS 在假管理计数器之前被拒绝", async (t) => {
  const app = await fixture(t);
  assert.equal(await upgrade(app), 403);
  assert.equal(app.evidence().counts.authorized, 0);
});

test("不同站点入口仅监听原生 IPv6 回环并可加载探测页", async (t) => {
  const app = await fixture(t);
  assert.equal(new URL(app.origins.crossSiteConsole).hostname, "[::1]");
  assert.equal((await fetch(`${app.origins.crossSiteConsole}/m/fixture`)).status, 200);
});

test("三个入口生成的浏览器脚本均可解析", async (t) => {
  const app = await fixture(t);
  for (const origin of Object.values(app.origins)) {
    const html = await fetch(`${origin}/m/fixture`).then((response) => response.text());
    const code = html.match(/<script>([\s\S]+)<\/script>/)?.[1];
    assert.ok(code);
    assert.doesNotThrow(() => new Script(code));
  }
});

test("新夹具不继承另一夹具的临时设备", async (t) => {
  const first = await fixture(t);
  const second = await fixture(t);
  const cookie = (await pair(first)).split(";")[0];
  const response = await fetch(`${second.origins.main}/m/api/v1/probe/foreign-device`, {
    headers: { cookie },
  });
  assert.equal(response.status, 403);
});

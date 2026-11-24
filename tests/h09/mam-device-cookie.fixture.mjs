// H09 §3.3 测试专用 Cookie 语义夹具：绝不启动 Tauri、真实 gate 或真实管理 API。
// 设备仅在 Map 中；HTTP/WS 只累加假管理计数器；不访问 home、SQLite、真实宿主或账本。
// 启动：node tests/h09/mam-device-cookie.fixture.mjs
// 用全新浏览器配置打开输出的 main URL，先完成正对照，再打开两个控制台逐项执行。
// Node 自测不是浏览器证据；同主机换端口预期出现授权命中，不能当成隔离方案。
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";

async function sourceContract() {
  const root = new URL("../../", import.meta.url);
  const gate = await readFile(new URL("src-tauri/src/remote/gate.rs", root), "utf8");
  const pairing = await readFile(new URL("src-tauri/src/remote/pairing.rs", root), "utf8");
  const name = gate.match(/pub const COOKIE_NAME: &str = "([^"]+)";/)?.[1];
  const template = pairing.match(/"(\{COOKIE\}=\{device_id\};[^"\n]+)"/)?.[1];
  const ttlExpression = pairing.match(/pub const DEVICE_TTL_MS: i64 = ([\d *]+);/)?.[1];
  assert.equal(name, "mam_device", "现库 Cookie 名已变化，须重新核对夹具");
  assert.equal(template, "{COOKIE}={device_id}; Path=/m; HttpOnly; SameSite=Lax; Max-Age={}");
  assert.ok(ttlExpression, "无法提取现库 TTL；禁止静默使用过期常量");
  const ttlMs = ttlExpression
    .split("*")
    .map(Number)
    .reduce((a, b) => a * b);
  assert.equal(ttlMs, 15552000000);
  assert.ok(pairing.includes("revoked == 0 && now - seen <= DEVICE_TTL_MS"));
  return {
    name,
    ttlMs,
    template,
    gateSha256: createHash("sha256").update(gate).digest("hex"),
    pairingSha256: createHash("sha256").update(pairing).digest("hex"),
  };
}

function page({ main, sameHostConsole, crossSiteConsole }, mode) {
  const run = randomBytes(8).toString("hex");
  return `<!doctype html><html lang="zh"><meta charset="utf-8"><title>H09 Cookie 语义夹具</title>
<h1>H09 Cookie 语义夹具</h1>
<p>只使用临时内存设备与假管理计数器。此页没有连接实际 Rust gate、Tauri 或真实账本。</p>
<p>请使用全新浏览器配置。主站先完成登录正对照，然后在同一浏览器逐个打开控制台。</p>
<p>当前页面 origin：<code id="origin"></code>；主站：<code>${main}</code></p>
<p>Cookie：mam_device；Path=/m；HttpOnly；SameSite=Lax；Max-Age=15552000；无 Domain/Secure。</p>
<button id="run">${mode === "main" ? "临时登录并执行同源 GET、POST、WS 正对照" : "执行当前控制台的跨 origin 请求"}</button>
<button id="refresh">刷新服务端计数</button>
<p><a href="${sameHostConsole}/m/fixture" target="_blank" rel="noopener">同主机不同端口控制台（反例，预期可借用 Cookie）</a></p>
<p><a href="${crossSiteConsole}/m/fixture" target="_blank" rel="noopener">不同站点控制台（[::1]，拒绝结果待实测）</a></p>
<p>跨站点不代表全部拓扑安全。Lax 顶层导航、同站点子域与四渠道映射均不由本夹具验收。</p>
<pre id="output">未执行。浏览器报错或 CORS 拦截不等于服务端拒绝。</pre>
<script>
const main = ${JSON.stringify(main)}, mode = ${JSON.stringify(mode)}, run = ${JSON.stringify(run)};
const output = document.getElementById('output');
document.getElementById('origin').textContent = location.origin;
const results = [];
const endpoint = kind => main + '/m/api/v1/probe/' + run + '/' + kind;
async function evidence() {
  const data = await fetch('/m/fixture/evidence', {cache: 'no-store'}).then(r => r.json());
  const rows = data.attempts.filter(row => row.path.includes('/' + run + '/'));
  const authorized = rows.filter(row => row.authorized).length;
  const required = ['cors-get', 'simple-post', 'no-cors-post', 'form-post', 'cross-ws'];
  const complete = required.every(kind => rows.some(row => row.path.endsWith('/' + kind)));
  let verdict;
  if (mode === 'main') {
    verdict = rows.length === 3 && authorized === 3 && results.some(row => row.opened)
      ? '本浏览器临时登录正对照成立（语义夹具）' : '正对照未成立，不能解释后续零命中';
  } else {
    verdict = authorized > 0 ? '反例成立：当前控制台 origin 产生了授权效果'
      : complete ? '本轮所列请求到达服务端且被拒绝（语义夹具，非产品验收）'
      : '证据不完整：缺少服务端请求日志，不能判隔离通过';
  }
  output.textContent = JSON.stringify({browser: navigator.userAgent, pageOrigin: location.origin,
    run, verdict, currentRun: {attempted: rows.length, authorized, rejected: rows.length - authorized},
    cookieVisibleToScript: document.cookie.includes('mam_device='), results, server: data,
    note: '没有请求日志 = 未到达服务端，不能称为 gate 拒绝；仅按 currentRun 判断本轮，历史正对照单列在 server 中'}, null, 2);
}
async function request(kind, init) {
  try {
    const response = await fetch(endpoint(kind), {...init, credentials: 'include'});
    results.push({kind, status: response.status, type: response.type});
  } catch (error) { results.push({kind, browserError: String(error)}); }
}
async function websocket(kind) {
  await new Promise(resolve => {
    const ws = new WebSocket(endpoint(kind).replace('http:', 'ws:'));
    const timer = setTimeout(() => { results.push({kind, timeout: true}); ws.close(); resolve(); }, 3000);
    ws.onopen = () => { results.push({kind, opened: true}); };
    ws.onerror = () => { results.push({kind, browserError: 'WebSocket error; inspect server evidence'}); };
    ws.onclose = event => { clearTimeout(timer); results.push({kind, closeCode: event.code}); resolve(); };
  });
}
async function form() {
  const frame = document.createElement('iframe');
  frame.name = 'h09-' + run; frame.hidden = true;
  await new Promise(resolve => { frame.onload = resolve; document.body.append(frame); });
  const form = document.createElement('form');
  form.method = 'POST'; form.action = endpoint('form-post'); form.target = frame.name;
  form.innerHTML = '<input name="fixture" value="counter-only">';
  document.body.append(form);
  await new Promise(resolve => {
    const timer = setTimeout(() => { results.push({kind: 'form-post', timeout: true}); resolve(); }, 3000);
    frame.onload = () => { clearTimeout(timer); results.push({kind: 'form-post', frameLoaded: true}); resolve(); };
    form.submit();
  });
  form.remove(); frame.remove();
}
document.getElementById('refresh').onclick = () => evidence().catch(e => { output.textContent = String(e); });
document.getElementById('run').onclick = async event => {
  event.target.disabled = true;
  try {
    if (mode === 'main') {
      const response = await fetch('/m/fixture/pair', {method: 'POST'});
      if (!response.ok) throw new Error('临时配对失败');
      await request('positive-get', {});
      await request('positive-post', {method: 'POST', body: 'counter-only', headers: {'Content-Type': 'text/plain'}});
      await websocket('positive-ws');
    } else {
      await request('cors-get', {});
      await request('json-post-preflight', {method: 'POST', body: '{}', headers: {'Content-Type': 'application/json'}});
      await request('simple-post', {method: 'POST', body: 'counter-only', headers: {'Content-Type': 'text/plain'}});
      await request('no-cors-post', {mode: 'no-cors', method: 'POST', body: 'counter-only', headers: {'Content-Type': 'text/plain'}});
      await form();
      await websocket('cross-ws');
    }
    await evidence();
  } catch (error) { output.textContent = String(error); }
};
</script></html>`;
}

export async function startFixture() {
  const contract = await sourceContract();
  const devices = new Map();
  const attempts = [];
  const servers = [];
  const sockets = new Set();
  const origins = {};
  const cookieFor = (id) =>
    contract.template
      .replace("{COOKIE}", contract.name)
      .replace("{device_id}", id)
      .replace("{}", String(contract.ttlMs / 1000));
  function evidence() {
    return {
      fixture: "Cookie semantics only; NOT actual Rust gate or browser verification",
      origins: { ...origins },
      contract,
      counts: {
        attempted: attempts.length,
        authorized: attempts.filter((row) => row.authorized).length,
        rejected: attempts.filter((row) => !row.authorized).length,
      },
      attempts: attempts.map((row) => ({ ...row })),
    };
  }
  function authorize(req, transport) {
    const raw = String(req.headers.cookie ?? "")
      .split(";")
      .map((s) => s.trim())
      .find((s) => s.startsWith(contract.name + "="));
    const id = raw?.slice(contract.name.length + 1).trim();
    const device = devices.get(id);
    const now = Date.now();
    const authorized = Boolean(
      device && !device.revoked && now - device.lastSeen <= contract.ttlMs
    );
    if (authorized) device.lastSeen = now;
    const row = {
      method: req.method,
      path: req.url,
      host: req.headers.host,
      origin: req.headers.origin ?? null,
      fetchSite: req.headers["sec-fetch-site"] ?? null,
      cookiePresent: Boolean(id),
      transport,
      authorized,
      status: authorized ? (transport === "ws" ? 101 : 200) : 403,
    };
    attempts.push(row);
    return row;
  }
  async function listen(host, mode) {
    const server = createServer((req, res) => {
      req.resume();
      res.setHeader("cache-control", "no-store");
      res.setHeader("x-content-type-options", "nosniff");
      if (req.url === "/m/fixture/evidence" && req.method === "GET") {
        res.setHeader("content-type", "application/json");
        return res.end(JSON.stringify(evidence()));
      }
      if (req.url === "/m/fixture" && req.method === "GET") {
        res.setHeader("content-type", "text/html; charset=utf-8");
        return res.end(page(origins, mode));
      }
      if (mode === "main" && req.url === "/m/fixture/pair" && req.method === "POST") {
        const id = randomBytes(16).toString("hex");
        devices.set(id, { revoked: false, lastSeen: Date.now() });
        res.setHeader("set-cookie", cookieFor(id));
        return res.end("temporary fixture device only");
      }
      if (mode === "main" && /^\/m\/api\/v1\/probe\/[a-zA-Z0-9/-]+$/.test(req.url)) {
        const row = authorize(req, "http");
        res.statusCode = row.status;
        return res.end(row.authorized ? "fake management counter incremented" : "");
      }
      res.statusCode = 404;
      res.end();
    });
    servers.push(server);
    server.on("connection", (socket) => {
      sockets.add(socket);
      socket.on("close", () => sockets.delete(socket));
    });
    server.on("upgrade", (req, socket) => {
      if (mode !== "main" || !/^\/m\/api\/v1\/probe\/[a-zA-Z0-9/-]+$/.test(req.url)) {
        return socket.end("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
      }
      if (!authorize(req, "ws").authorized) {
        return socket.end(
          "HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        );
      }
      const accept = createHash("sha1")
        .update(String(req.headers["sec-websocket-key"]) + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
        .digest("base64");
      socket.write(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " +
          accept +
          "\r\n\r\n"
      );
      socket.end(Buffer.from([0x88, 0x00]));
    });
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(0, host, resolve);
    });
    return `http://${host.includes(":") ? `[${host}]` : host}:${server.address().port}`;
  }
  async function close() {
    for (const socket of sockets) socket.destroy();
    await Promise.all(
      servers.map(
        (server) =>
          new Promise((resolve, reject) => {
            if (!server.listening) return resolve();
            server.close((error) => (error ? reject(error) : resolve()));
          })
      )
    );
    devices.clear();
  }
  try {
    origins.main = await listen("127.0.0.1", "main");
    origins.sameHostConsole = await listen("127.0.0.1", "console");
    origins.crossSiteConsole = await listen("::1", "console");
    return { origins, evidence, close };
  } catch (error) {
    await close();
    throw error;
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const app = await startFixture();
  console.log(
    JSON.stringify(
      { main: app.origins.main + "/m/fixture", ...app.evidence(), browserExecuted: false },
      null,
      2
    )
  );
  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.once(signal, async () => {
      await app.close();
      process.exit(0);
    });
  }
}

// 仅验证新浏览器页的报告逻辑；不运行 Rust/Cargo，也不冒充浏览器实测。
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { Script, createContext } from "node:vm";

const html = await readFile(new URL("./mam-device-rust-gate.html", import.meta.url), "utf8");
assert.equal(html.split("__FIXTURE_CONFIG__").length, 2, "配置占位必须唯一");
const config = {
  mode: "main",
  run: "node-report-test",
  cookieTemplate: "mam_device=REDACTED; Path=/m; HttpOnly; SameSite=Lax; Max-Age=15552000",
  origins: {
    main: "http://127.0.0.1:19001",
    sameHostConsole: "http://127.0.0.1:19002",
    crossSiteConsole: "http://[::1]:19003",
  },
};
const source = html
  .replace("__FIXTURE_CONFIG__", JSON.stringify(config))
  .match(/<script>([\s\S]+)<\/script>/)?.[1];
assert.ok(source);
const nodes = new Map();
const context = createContext({
  document: {
    getElementById(id) {
      if (!nodes.has(id)) nodes.set(id, {});
      return nodes.get(id);
    },
  },
  location: { origin: config.origins.main },
});
new Script(source).runInContext(context);
const verdict = context.verdict;
const rejectedRows = ["cors-get", "simple-post", "no-cors-post", "form-post", "cross-ws"].map(
  (kind) => ({
    case: kind,
    method: kind.endsWith("post") ? "POST" : "GET",
    status: 403,
    authorized: false,
  })
);
const options = { case: "json-post-preflight", method: "OPTIONS", status: 403, authorized: false };

function positiveRows() {
  return [
    { case: "positive-get", status: 200, authorized: true },
    { case: "positive-post", status: 200, authorized: true },
    { case: "positive-ws", status: 101, authorized: true },
  ];
}
function positiveBrowserResults() {
  return [
    { kind: "positive-get", status: 200 },
    { kind: "positive-post", status: 200 },
    { kind: "positive-ws", opened: true },
  ];
}

test("模板替换后的浏览器脚本可初始化三个入口", () => {
  assert.equal(nodes.get("same-host").href, config.origins.sameHostConsole + "/m/fixture");
  assert.equal(nodes.get("cross-site").href, config.origins.crossSiteConsole + "/m/fixture");
  assert.equal(typeof nodes.get("run").onclick, "function");
});

test("正对照同时要求实际处理器记录和浏览器 HTTP/WS 成功", () => {
  assert.match(
    verdict("main", positiveRows(), positiveBrowserResults(), true, false),
    /正对照成立/
  );
});

test("浏览器 WS 未打开时不把服务端 101 单独判为正对照成立", () => {
  const results = positiveBrowserResults().filter((row) => row.kind !== "positive-ws");
  assert.match(verdict("main", positiveRows(), results, true, false), /正对照未成立/);
});

test("Cookie 对脚本可见时正对照失败", () => {
  assert.match(
    verdict("main", positiveRows(), positiveBrowserResults(), true, true),
    /正对照未成立/
  );
});

test("缺少本服务登录正对照时不能判控制台隔离通过", () => {
  assert.match(
    verdict("console", [...rejectedRows, options], [], false, false),
    /缺少.*登录正对照/
  );
});

test("即使浏览器报告 CORS 错误服务端授权命中仍判反例成立", () => {
  const rows = [{ case: "simple-post", status: 200, authorized: true }];
  assert.match(verdict("console", rows, [{ browserError: "CORS" }], true, false), /反例成立/);
});

test("零授权且没有服务端请求记录时判证据不完整", () => {
  assert.match(verdict("console", [], [], true, false), /证据不完整/);
});

test("缺少 JSON 预检拒绝记录时不能判整轮拒绝成立", () => {
  assert.match(verdict("console", rejectedRows, [], true, false), /证据不完整/);
});

test("JSON POST 已到达时不能声称它止于预检", () => {
  const post = { ...options, method: "POST" };
  assert.match(verdict("console", [...rejectedRows, options, post], [], true, false), /证据不完整/);
});

test("拒绝记录齐全也只给出当前测试拓扑结论", () => {
  assert.match(
    verdict("console", [...rejectedRows, options], [], true, false),
    /仅本地测试拓扑，不代表 H09 验收/
  );
});

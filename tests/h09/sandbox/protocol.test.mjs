import assert from "node:assert/strict";
import { test } from "node:test";
import { brokerDecision, judge } from "./protocol.mjs";

const report = {
  type: "h09-report",
  run: "test-run",
  phase: "complete",
  results: { module: true },
};
const event = (data) => ({ sourceMatches: true, origin: "null", run: "test-run", data });

test("仅接受已登记 opaque 子窗口的严格报告消息", () => {
  assert.equal(brokerDecision(event(report)).accepted, true);
});
test("同为 null origin 的其他窗口不能借用消息代理", () => {
  assert.equal(brokerDecision({ ...event(report), sourceMatches: false }).accepted, false);
});
test("非 opaque origin 的消息被拒绝", () => {
  assert.equal(brokerDecision({ ...event(report), origin: "http://127.0.0.1" }).accepted, false);
});
test("请求代发 fetch 的消息被拒绝", () => {
  assert.equal(brokerDecision(event({ type: "fetch", url: "/m/api/v1/settings" })).accepted, false);
});
test("伪装成报告的 URL 和方法字段不能进入可信页", () => {
  assert.equal(
    brokerDecision(event({ ...report, url: "/m/api/v1/settings", method: "POST" })).accepted,
    false
  );
});
test("其他轮次的报告被拒绝", () => {
  assert.equal(brokerDecision(event({ ...report, run: "old-run" })).accepted, false);
});
test("报告中夹带请求字段时被拒绝", () => {
  assert.equal(
    brokerDecision(event({ ...report, results: { fetch: true, url: "/m/api/v1/settings" } }))
      .accepted,
    false
  );
});

function evidence() {
  return {
    management: [
      ...["positive-get", "positive-post", "positive-ws", "navigation-control"].map((kind) => ({
        case: kind,
        authorized: true,
        status: kind === "positive-ws" ? 101 : 200,
      })),
      ...[
        "simple-post",
        "no-cors-post",
        "capability-query",
        "form-post",
        "attack-ws",
        "navigation-self",
      ].map((kind) => ({
        case: kind,
        authorized: false,
        status: 403,
      })),
    ],
    console: [
      ...["module", "fetch", "ws", "upload", "download-fetch", "download-link"].map((kind) => ({
        kind,
        authorized: true,
        origin: "null",
        cookiePresent: false,
        status: kind === "ws" ? 101 : 200,
      })),
      { kind: "ws-expired", authorized: false, status: 1008 },
    ],
    child: {
      module: true,
      fetch: true,
      ws: true,
      upload: true,
      downloadBytes: true,
      parentDomDenied: true,
      topNavigationDenied: true,
      popupDenied: true,
    },
    opaqueReportSeen: true,
    parentStable: true,
    sandbox: "allow-scripts allow-forms allow-downloads",
    brokerRejected: 4,
    downloadConfirmed: true,
    expiredHttpRejected: true,
    expiredWsClosed: true,
  };
}

test("主站已登录后有任意攻击授权命中即判反例", () => {
  const data = evidence();
  data.management.push({ case: "simple-post", status: 200, authorized: true });
  assert.equal(judge(data).state, "failed");
});
test("缺少登录正对照时零命中不能判通过", () => {
  const data = evidence();
  data.management = data.management.filter((row) => row.case !== "positive-ws");
  assert.equal(judge(data).state, "incomplete");
});
test("仅上传下载的客户端成功声明不能代替服务端记录", () => {
  const data = evidence();
  data.console = [];
  assert.equal(judge(data).state, "incomplete");
});
test("缺少导航拒绝记录时不能由 sandbox 属性推定安全", () => {
  const data = evidence();
  data.management = data.management.filter((row) => row.case !== "navigation-self");
  assert.equal(judge(data).state, "incomplete");
});
test("未核对实际下载文件时不把发起下载判为可用性通过", () => {
  assert.equal(judge({ ...evidence(), downloadConfirmed: false }).state, "incomplete");
});
test("短期能力未验证 HTTP 失效及活 WS 关闭时保留缺口", () => {
  assert.equal(judge({ ...evidence(), expiredWsClosed: false }).state, "incomplete");
});
test("误加 allow-same-origin 时立即判失败", () => {
  assert.equal(
    judge({ ...evidence(), sandbox: "allow-scripts allow-same-origin" }).state,
    "failed"
  );
});
test("console 意外携带主站设备 Cookie 时不能判候选成立", () => {
  const data = evidence();
  data.console[0].cookiePresent = true;
  assert.equal(judge(data).state, "failed");
});
test("资源请求不是 opaque origin 时保留可用性缺口", () => {
  const data = evidence();
  data.console[0].origin = "http://127.0.0.1";
  assert.equal(judge(data).state, "incomplete");
});
test("no-cors 请求没有到达服务端时不能判整轮隔离成立", () => {
  const data = evidence();
  data.management = data.management.filter((row) => row.case !== "no-cors-post");
  assert.equal(judge(data).state, "incomplete");
});
test("子页声称 WS 已到期关闭时仍须有服务器关闭记录", () => {
  const data = evidence();
  data.console = data.console.filter((row) => row.kind !== "ws-expired");
  assert.equal(judge(data).state, "incomplete");
});
test("证据齐全也只给本地测试机制结论", () => {
  const result = judge(evidence());
  assert.equal(result.state, "local-evidence-complete");
  assert.match(result.message, /不代表 H09/);
});

import { brokerDecision, judge } from "./protocol.mjs";
const output = document.getElementById("output");
const initialHref = location.href;
const run = crypto.randomUUID();
const flags = "allow-scripts allow-forms allow-downloads";
const frames = [];
const broker = [];
const child = {};
let grant,
  latest,
  opaqueReportSeen = false,
  downloadConfirmed = false,
  expiredHttpRejected = false;
const trusted = new URL(location.href).searchParams.get("trusted") === "1";
const button = (id) => document.getElementById(id);
const managementUrl = (kind) => "/m/api/v1/probe/" + run + "/" + kind;
function render() {
  if (!latest) {
    output.textContent = "等待服务端证据；run=" + run;
    return;
  }
  const management = latest.management.attempts.filter((row) => row.run === run);
  const consoleRows = latest.console.filter((row) => row.run === run);
  const state = {
    management,
    console: consoleRows,
    child,
    opaqueReportSeen,
    parentStable: location.href === initialHref && window.top === window && window.opener === null,
    sandbox: frames.map((frame) => frame.getAttribute("sandbox")).every((value) => value === flags)
      ? flags
      : "allow-same-origin",
    brokerRejected: broker.filter((row) => !row.accepted).length,
    downloadConfirmed,
    expiredHttpRejected,
    expiredWsClosed: child.expiredWsClosed === true,
  };
  output.textContent = JSON.stringify(
    {
      observedAt: new Date().toISOString(),
      browser: navigator.userAgent,
      origin: location.origin,
      run,
      verdict: judge(state),
      ...state,
      broker,
      expiresAt: grant?.expiresAt,
      note: "opaque 子页报告须与服务端计数一起读；未到达服务器的请求不能称为 gate 拒绝。",
    },
    null,
    2
  );
}
async function refresh() {
  const response = await fetch("/m/fixture/sandbox/evidence", {
    cache: "no-store",
    signal: AbortSignal.timeout(5000),
  });
  if (!response.ok) throw new Error("证据读取失败");
  latest = await response.json();
  render();
}
function frame(navigate = false) {
  const node = document.createElement("iframe");
  node.setAttribute("sandbox", flags);
  node.setAttribute("referrerpolicy", "no-referrer");
  node.title = navigate ? "隔离子页自身导航探测" : "opaque 控制台机制探测";
  node.width = "900";
  node.height = navigate ? "100" : "240";
  frames.push(node);
  node.src = grant.base + "index.html" + (navigate ? "?navigate=1" : "");
  document.getElementById("frames").append(node);
}
window.addEventListener("message", (event) => {
  // opaque origin 都显示 null，必须同时核对 WindowProxy、轮次及严格消息形状。
  const decision = brokerDecision({
    sourceMatches: frames.some((node) => event.source === node.contentWindow),
    origin: event.origin,
    run,
    data: event.data,
  });
  broker.push({ accepted: decision.accepted, reason: decision.reason ?? decision.phase });
  if (decision.accepted) {
    opaqueReportSeen = true;
    Object.assign(child, decision.results);
    if (decision.phase === "complete" && frames.length === 1) frame(true);
  }
  // 没有代发网络、导航、存储或凭据访问；只合并固定布尔报告并重绘已有证据。
  render();
});
async function baselineWs() {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(
      location.origin.replace("http:", "ws:") + "/m/api/v1/ws/" + run + "/positive-ws"
    );
    const timer = setTimeout(() => {
      ws.close();
      reject(new Error("主站 WS 正对照超时"));
    }, 5000);
    ws.onopen = () => {
      clearTimeout(timer);
      resolve();
    };
    ws.onerror = () => {
      clearTimeout(timer);
      reject(new Error("主站 WS 正对照失败"));
    };
  });
}
async function navigationControl() {
  const node = document.createElement("iframe");
  node.hidden = true;
  try {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("无 sandbox 的导航正对照超时")), 5000);
      node.onload = () => {
        clearTimeout(timer);
        resolve();
      };
      node.src = managementUrl("navigation-control");
      document.body.append(node);
    });
  } finally {
    node.remove();
  }
}
button("launch").onclick = () =>
  window.open("/m/fixture/sandbox?trusted=1", "_blank", "noopener,noreferrer");
if (trusted) {
  button("launch").hidden = true;
  button("start").hidden = false;
  document.getElementById("instructions").textContent =
    "先点临时登录；随后点击子页的运行按钮，再点击子页下载链接。核对文件后确认下载，120秒后检查过期并刷新。";
}
button("start").onclick = async () => {
  button("start").disabled = true;
  try {
    if (window.opener !== null || window.top !== window)
      throw new Error("必须在无 opener 的可信顶层新页运行");
    if (!(await fetch("/m/fixture/pair", { method: "POST" })).ok) throw new Error("临时登录失败");
    for (const [kind, init] of [
      ["positive-get", {}],
      ["positive-post", { method: "POST", body: "counter-only" }],
    ]) {
      if ((await fetch(managementUrl(kind), init)).status !== 200)
        throw new Error("主站正对照失败：" + kind);
    }
    await baselineWs();
    await navigationControl();
    const response = await fetch("/m/api/v1/sandbox/" + run + "/issue", { method: "POST" });
    if (!response.ok) throw new Error("实际 gate 拒绝能力签发");
    grant = await response.json();
    frame();
    for (const id of ["refresh", "download-confirm", "expiry"]) button(id).hidden = false;
    await refresh();
  } catch (error) {
    output.textContent = String(error);
  }
};
button("refresh").onclick = () =>
  refresh().catch((error) => {
    output.textContent = String(error);
  });
button("download-confirm").onclick = async () => {
  downloadConfirmed = true;
  await refresh();
};
button("expiry").onclick = async () => {
  try {
    if (Date.now() < grant.expiresAt)
      throw new Error(
        "能力尚未到期，剩余秒数：" + Math.ceil((grant.expiresAt - Date.now()) / 1000)
      );
    expiredHttpRejected =
      (await fetch(grant.base + "fetch.json", { credentials: "omit", cache: "no-store" }))
        .status === 403;
    await refresh();
  } catch (error) {
    output.textContent = String(error);
  }
};

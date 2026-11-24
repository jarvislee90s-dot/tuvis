// 仅供测试可信页使用：消息永不转为 fetch/导航/凭据访问。
const resultKeys = new Set([
  "module",
  "fetch",
  "ws",
  "upload",
  "downloadBytes",
  "parentDomDenied",
  "topNavigationDenied",
  "popupDenied",
  "expiredWsClosed",
  "navigationStarted",
]);
export function brokerDecision({ sourceMatches, origin, run, data }) {
  const deny = (reason) => ({ accepted: false, reason });
  if (!sourceMatches) return deny("unregistered-window");
  if (origin !== "null") return deny("non-opaque-origin");
  if (!data || typeof data !== "object" || Array.isArray(data)) return deny("invalid-object");
  if (Object.keys(data).sort().join(",") !== "phase,results,run,type")
    return deny("unexpected-fields");
  if (data.type !== "h09-report" || data.run !== run) return deny("wrong-type-or-run");
  if (!["complete", "navigation-started", "expired-ws"].includes(data.phase))
    return deny("unknown-phase");
  if (!data.results || typeof data.results !== "object" || Array.isArray(data.results))
    return deny("invalid-results");
  if (
    !Object.entries(data.results).every(
      ([key, value]) => resultKeys.has(key) && typeof value === "boolean"
    )
  )
    return deny("unexpected-result");
  return { accepted: true, phase: data.phase, results: { ...data.results } };
}

export function judge(data) {
  const missing = [];
  const sandbox = new Set(data.sandbox.split(/\s+/));
  if (
    [
      "allow-same-origin",
      "allow-top-navigation",
      "allow-top-navigation-by-user-activation",
      "allow-popups",
      "allow-popups-to-escape-sandbox",
    ].some((flag) => sandbox.has(flag))
  ) {
    return { state: "failed", message: "sandbox 授予了候选外权限" };
  }
  const baseline = new Set(["positive-get", "positive-post", "positive-ws", "navigation-control"]);
  if (data.management.some((row) => row.authorized && !baseline.has(row.case))) {
    return { state: "failed", message: "出现主站设备权限借用：攻击请求命中实际 gate 后的处理器" };
  }
  for (const kind of baseline) {
    if (
      !data.management.some(
        (row) =>
          row.case === kind && row.authorized && row.status === (kind === "positive-ws" ? 101 : 200)
      )
    )
      missing.push("baseline:" + kind);
  }
  for (const kind of [
    "simple-post",
    "no-cors-post",
    "capability-query",
    "form-post",
    "attack-ws",
    "navigation-self",
  ]) {
    if (!data.management.some((row) => row.case === kind && !row.authorized && row.status === 403))
      missing.push("server-rejection:" + kind);
  }
  if (data.console.some((row) => row.cookiePresent === true))
    return { state: "failed", message: "console 路径意外携带主站设备 Cookie" };
  for (const kind of ["module", "fetch", "ws", "upload", "download-fetch", "download-link"]) {
    if (
      !data.console.some(
        (row) => row.kind === kind && row.authorized && row.status === (kind === "ws" ? 101 : 200)
      )
    )
      missing.push("console:" + kind);
    if (
      kind !== "download-link" &&
      !data.console.some((row) => row.kind === kind && row.authorized && row.origin === "null")
    )
      missing.push("opaque-console-origin:" + kind);
  }
  for (const kind of [
    "module",
    "fetch",
    "ws",
    "upload",
    "downloadBytes",
    "parentDomDenied",
    "topNavigationDenied",
    "popupDenied",
  ]) {
    if (data.child[kind] !== true) missing.push("browser:" + kind);
  }
  if (!data.opaqueReportSeen) missing.push("opaque-origin-report");
  if (!data.parentStable) return { state: "failed", message: "可信父页发生了非预期导航" };
  if (data.brokerRejected < 4) missing.push("broker-counterexamples");
  if (!data.downloadConfirmed) missing.push("actual-downloaded-file-confirmation");
  if (!data.expiredHttpRejected) missing.push("expired-capability-http");
  if (
    !data.expiredWsClosed ||
    !data.console.some((row) => row.kind === "ws-expired" && row.status === 1008 && !row.authorized)
  )
    missing.push("expired-capability-live-ws");
  return missing.length
    ? { state: "incomplete", message: "证据不足，不判通过", missing }
    : {
        state: "local-evidence-complete",
        message: "本地测试机制所列证据齐全；不代表 H09 或真实官方页面已验收",
      };
}

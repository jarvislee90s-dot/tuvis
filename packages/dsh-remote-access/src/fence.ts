/**
 * 逐路由访问门禁（N1）——官方围栏语义的插件侧复刻。
 *
 * **为什么必须自带**（Mac 补探测报告 §3.5 实测）：官方认证与 Host/Origin 围栏
 * 只装在 `dsh-client-connection` 自己注册的路径面上（`/`、`/connect`、`/api/*`、
 * mux）；经 `webServer.register` 自注册的插件路由**不继承**任何门禁——伪装
 * Host/Origin 的请求照样 200。同一份报告 §4.4/§11.3 又用 Desktop 宿主的单变量
 * 矩阵定死了官方判据，因此本模块逐条复刻该判据（而不是另发明一套更松的）：
 *
 * | 条件 | 判据（与 §11.3 逐格对应） |
 * | --- | --- |
 * | Host | 解析出的**主机名**必须是回环字面量：127.0.0.0/8、`::1`（含 `[::1]`）、`localhost`；**端口不参与判定** |
 * | Origin 存在 | 其 authority 必须与 Host 头**逐字相同**（含端口、大小写按字面）；`null` 或非绝对 URL → 拒 |
 * | Origin 缺失 | 放行（浏览器顶层导航不带 Origin）——此时只受 Host 回环约束 |
 * | sec-fetch-site | `cross-site` → 拒；其余（`same-origin`/`same-site`/`none`/缺失）不额外拦 |
 * | 代理头 | `X-Forwarded-*` **不参与**判定（与官方一致） |
 *
 * **已知限度（如实申报，不夸大）**：Host 判定不校验端口、也不要求等于绑定字面量，
 * 因此它**不是** DNS-rebinding 的完整防线——与官方围栏同一限度。兔维斯侧真正
 * 的连接目标钉定在 Rust 传输层（只连已验证的回环 authority）。本门禁的职责是
 * 让「插件路由比宿主更松」这个差异消失，并拦住跨源页面的读取尝试。
 *
 * **不变量**：本模块只做**读**判定。插件路由不得新增任何授权写入口——批准
 * 永远在兔维斯 Tauri IPC 侧（见 index.ts 的路由表注释）。
 */

/** 门禁判定结果（拒绝时只带固定短原因，绝不回声攻击者输入）。 */
export type FenceVerdict = { allowed: true } | { allowed: false; reason: string };

/** 请求头最小面（node 的 `req.headers` 已小写化；此处只取判定需要的键）。 */
export interface FenceHeaders {
  host?: string | undefined;
  origin?: string | undefined;
  "sec-fetch-site"?: string | undefined;
}

/** 把 `host[:port]` / `[v6][:port]` 拆成主机名（端口丢弃，端口不参与判定）。 */
function hostnameOf(authority: string): string {
  const trimmed = authority.trim();
  if (trimmed.startsWith("[")) {
    const end = trimmed.indexOf("]");
    // 方括号形态：[::1] 或 [::1]:51583
    return end >= 0 ? trimmed.slice(1, end).toLowerCase() : trimmed.toLowerCase();
  }
  // 只有一个冒号才是 host:port；裸 IPv6（无方括号）不会被这里拆错（官方也要求方括号）
  const colon = trimmed.lastIndexOf(":");
  return (colon >= 0 ? trimmed.slice(0, colon) : trimmed).toLowerCase();
}

/** 是否为回环主机名（127.0.0.0/8、`::1`、`localhost`，含 IPv4-mapped 形态）。 */
function isLoopbackHostname(hostname: string): boolean {
  if (hostname === "localhost") return true;
  if (hostname === "::1") return true;
  // IPv4-mapped IPv6：::ffff:127.0.0.1 / ::ffff:7f00:1（官方 isLoopbackHost 同款放宽）
  const mapped = /^::ffff:(.+)$/.exec(hostname);
  if (mapped !== null) {
    const inner = mapped[1]!;
    if (/^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(inner)) return true;
    // 十六进制缩写形态（::ffff:7f00:1）
    return /^7f[0-9a-f]{2}:[0-9a-f]{1,4}$/.test(inner);
  }
  const ipv4 = /^127\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(hostname);
  if (ipv4 === null) return false;
  return [ipv4[1], ipv4[2], ipv4[3]].every((part) => Number(part) <= 255);
}

/** Origin 的 authority 是否与 Host 头逐字相同（含端口）。 */
function originMatchesHost(origin: string, host: string): boolean {
  // 官方纪律：Origin 存在即必须逐字同期；`null` 一律拒
  if (origin === "null" || origin.length === 0) return false;
  let parsed: URL;
  try {
    parsed = new URL(origin);
  } catch {
    return false;
  }
  // 只接受 http/https 绝对 URL（file:/data: 等不是浏览器来源）
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return false;
  // authority 逐字比对（parsed.host 已去掉默认端口，故先用原始串切出 authority）
  const afterScheme = origin.slice(origin.indexOf("://") + 3);
  const slash = afterScheme.search(/[/?#]/);
  const authority = slash >= 0 ? afterScheme.slice(0, slash) : afterScheme;
  return authority === host.trim();
}

/**
 * 对一次请求做门禁判定。
 *
 * @param headers - 至少含 `host`（HTTP/1.1 必有；HTTP/1.0 缺失即拒）。
 * @returns 放行或带固定短原因的拒绝。
 */
export function fenceRequest(headers: FenceHeaders): FenceVerdict {
  const host = headers.host?.trim() ?? "";
  if (host.length === 0) return { allowed: false, reason: "missing host" };

  if (!isLoopbackHostname(hostnameOf(host))) {
    // 不回显 Host 值（响应体不含攻击者输入）
    return { allowed: false, reason: "host is not loopback" };
  }

  const origin = headers.origin;
  if (origin !== undefined && !originMatchesHost(origin, host)) {
    return { allowed: false, reason: "origin does not match host" };
  }

  if ((headers["sec-fetch-site"] ?? "").toLowerCase() === "cross-site") {
    return { allowed: false, reason: "cross-site request" };
  }

  return { allowed: true };
}

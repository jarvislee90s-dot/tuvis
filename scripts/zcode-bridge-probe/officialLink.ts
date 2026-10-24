// scripts/zcode-bridge-probe/officialLink.ts — 官方远控链接解析（B1 实连半场）。
//
// 归属：工作包 B 探针工具目录，不在产品运行链上（不被 src/ 引用）。
// 职责：把「用户经 Tuvis A4 设置卡保存的官方链接」或「--sid 现场输入」解析为
// auth_init 需要的 device_sid（sid）与可选 device_mid（mid）。
//
// 校验口径（产品侧 A4 合同 + 官方解析器，逐条行号）：
//   - 存储 key `remote.zcode_official_url`、官方主机精确 `zcode.z.ai`、sid ≤128 字节、
//     总长 ≤4096、无 userinfo、仅默认 443 端口、已知身份字段不得重复
//     → src-tauri/src/services/zcode_control/config.rs:17-27（本仓库实现）
//   - 链接形状 `{origin}/remote/vN?sid&mid&name&app_version`；带 legacy `hash`/`t`
//     参数直接拒收 → 官方解析器（固定 SHA 3c5b2276）
//     packages/shared/src/web-remote-control.ts:163-207
//   - vN 版本门控 v3/v4 → packages/shared/src/zcodeEndpoint.ts:371-377
//
// 语法纪律：本目录文件需可被 `node`（≥22.6 type-stripping）直接执行——只用可擦除
// TS 语法（无 enum/namespace/参数属性），相对导入带 .ts 扩展名。

export type OfficialLinkRejectReason =
  | "parse"
  | "scheme"
  | "host"
  | "path"
  | "port"
  | "userinfo"
  | "too-long"
  | "sid-missing"
  | "sid-duplicate"
  | "sid-invalid"
  | "identity-duplicate"
  | "mid-invalid"
  | "legacy-param";

export type ParsedOfficialLink =
  | {
      ok: true;
      /** 规范化 origin（如 https://zcode.z.ai）——relay HTTP/WS 的派生基座。 */
      origin: string;
      host: string;
      pathVersion: "v3" | "v4";
      /** device_sid：auth_init 的目标绑定（relay contract.ts:19-24）。 */
      sid: string;
      /** device_mid：可选；WS URL 可带（contract.ts:102 注；transport.ts:106-110）。 */
      mid: string | null;
      name: string | null;
      appVersion: string | null;
    }
  | { ok: false; reason: OfficialLinkRejectReason };

/** 官方主机（精确匹配，A4：config.rs:20——不接受子域/相似域）。 */
export const OFFICIAL_LINK_HOST = "zcode.z.ai";
/** 链接总长上限（config.rs:22）。 */
const MAX_LINK_BYTES = 4096;
/** sid/mid 的 UTF-8 字节上限（config.rs:23-24）。 */
const MAX_SID_BYTES = 128;
/** 官方解析器直接拒收的 legacy 参数（web-remote-control.ts:191-207）。 */
const LEGACY_PARAMS = ["hash", "t"] as const;

function byteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

/**
 * 解析并复验官方远控链接（探针侧第二道防线：产品 A4 已验过，这里不信任输入）。
 * 返回 either 解析产物 or 结构化拒绝原因（不含链接原文）。
 */
export function parseOfficialLink(raw: string): ParsedOfficialLink {
  const trimmed = raw.trim();
  if (byteLength(trimmed) > MAX_LINK_BYTES) return { ok: false, reason: "too-long" };
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return { ok: false, reason: "parse" };
  }
  if (url.protocol !== "https:") return { ok: false, reason: "scheme" };
  if (url.username || url.password) return { ok: false, reason: "userinfo" };
  if (url.hostname.toLowerCase() !== OFFICIAL_LINK_HOST) return { ok: false, reason: "host" };
  if (url.port) return { ok: false, reason: "port" };
  const pathMatch = /^\/remote\/v(3|4)\/?$/.exec(url.pathname);
  if (!pathMatch) return { ok: false, reason: "path" };
  const params = url.searchParams;
  for (const legacy of LEGACY_PARAMS) {
    if (params.has(legacy)) return { ok: false, reason: "legacy-param" };
  }
  const sids = params.getAll("sid");
  if (sids.length === 0) return { ok: false, reason: "sid-missing" };
  if (sids.length > 1) return { ok: false, reason: "sid-duplicate" };
  const sid = sids[0];
  if (!sid || byteLength(sid) > MAX_SID_BYTES) return { ok: false, reason: "sid-invalid" };
  const mids = params.getAll("mid");
  if (mids.length > 1) return { ok: false, reason: "identity-duplicate" };
  const mid = mids[0] ?? null;
  if (mid !== null && (!mid || byteLength(mid) > MAX_SID_BYTES)) {
    return { ok: false, reason: "mid-invalid" };
  }
  return {
    ok: true,
    origin: url.origin,
    host: url.hostname,
    pathVersion: pathMatch[1] === "4" ? "v4" : "v3",
    sid,
    mid,
    name: params.get("name"),
    appVersion: params.get("app_version"),
  };
}

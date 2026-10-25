// scripts/zcode-bridge-probe/auth-check.ts — ZCode 官方桥接「真实连接」探针入口（B1 实连半场）。
//
// 归属：工作包 B 探针工具目录（不在产品运行链上：不被 src/ 引用、不挂应用启动/扫描）。
// 用途（controller 与用户在场交互执行；本仓库开发过程不代跑）：
//   1. probe-oauth   —— 验证官方授权服务器是否接受 Tuvis 的本地回环 redirect_uri：
//      起 127.0.0.1 随机端口回调监听 → 打印授权 URL（用户本人浏览器完成授权，探针绝不
//      代登/不碰密码）→ 有界等回调 → state 校验 → code 换账号 JWT → PROVEN /
//      UNSUPPORTED（结构化、脱敏）定案。JWT 只存内存。
//   2. connect-readonly —— 仅当上一步在进程内持有 JWT（或 --jwt-stdin 重跑）：
//      取已保存官方链接的 sid/mid（--sid 或只读打开 ~/.tuvis/tuvis.db）→ 申请 role=terminal
//      一次性 token（两次申请值必须不同）→ 单条 WS 连接 auth_init→auth_ack（waiting =
//      终端尚未可路由到设备：缺 mid/设备离线；官方无 APP 侧配对确认，30s 路由窗口自动
//      超时）→ paired 后**只发** bootstrap-request / workspace-list-request 两个只读查询 →
//      主动断开。
//   3. 证据：脱敏 transcript.json + run.md 写入 ~/Documents/tuvis-verify-logs/zcode/
//      的私有运行目录（0600）；真实值经 SecretRedactor fail-closed 闸，绝不入仓库/stdout。
//
// wire 依据 = 固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// （逐条行号见各被调模块头注与决策文件 §6.2）：
//   - 授权入口按 --provider 通道分派（--provider=zai|bigmodel，默认 zai）：
//     zai → `https://chat.z.ai/api/oauth/authorize`（webZaiOAuthConfig.ts:30）；
//     bigmodel → `https://bigmodel.cn/login?redirect=<回调>&appId=zcode&state=<state>`
//     （zaiWebOAuthProvider.ts:175-181；webZaiOAuthConfig.ts:40-42,60-61；zcodeEndpoint.ts:6,252-264）。
//   - 公开 client_id `client_P8X5CMWmlaRO9gyO-KSqtg`（webZaiOAuthConfig.ts:57-58）——仅 zai 通道；
//     bigmodel 通道用 appId=zcode（webZaiOAuthConfig.ts:60-61）。
//   - 交换（两通道共用，仅 provider 字面量不同：oauth.ts:9,12）：
//     `POST https://zcode.z.ai/api/v1/oauth/token`（webZaiOAuthConfig.ts:56 同源相对路径）
//   - **relay 主机派生**：与 ZCode 端点同源主机——`buildZCodeEndpointUrls` 把同一 origin 的
//     协议换成 ws(s) 后拼 `/api/v1/remote/ws2`（packages/shared/src/zcodeEndpoint.ts:369-370,380；
//     默认常量 `wss://zcode.z.ai/api/v1/remote/ws2` zcodeEndpoint.ts:14；apiBaseUrl 同 origin
//     zcodeEndpoint.ts:376）。即 relay host = 官方链接/端点 origin 的 host（zcode.z.ai），
//     token 端点 = `https://<同主机>/api/v1/remote/token`。
//   - 只读查询（paired 后官方终端同款）：bootstrap-request / workspace-list-request
//     （packages/web/src/web-remote-control/app/main.tsx:1504-1514,1572-1591）。
//
// 运行方式：`node scripts/zcode-bridge-probe/auth-check.ts <flags>`（Node ≥22.22 的
// type-stripping 直接执行；本目录只用可擦除 TS 语法、内置依赖，无 tsx/新依赖）。

import * as childProcess from "node:child_process";
import * as os from "node:os";
import * as path from "node:path";
import { pathToFileURL } from "node:url";
import { ReadonlyPayloadChannel } from "./appPayload.ts";
import { parseOfficialLink } from "./officialLink.ts";
import {
  LoopbackCallbackServer,
  OFFICIAL_AUTHORIZE_ORIGIN,
  OFFICIAL_BIGMODEL_AUTHORIZE_ORIGIN,
  OFFICIAL_BIGMODEL_AUTHORIZE_PATH,
  OFFICIAL_BIGMODEL_APP_ID,
  OFFICIAL_OAUTH_CLIENT_ID,
  OFFICIAL_ZCODE_ENDPOINT_ORIGIN,
  OFFICIAL_AUTHORIZE_PATH,
  buildAuthorizeUrl,
  exchangeCodeForToken,
  generateOAuthState,
  type LoopbackCallback,
  type OAuthProviderChannel,
} from "./oauthFlow.ts";
import { RELAY_WS_PATH, RelayTerminalConnection, requestTerminalToken } from "./relayAuth.ts";
import {
  SecretRedactor,
  createEvidenceRunDir,
  maskJwt,
  maskOpaque,
  writeEvidenceFiles,
} from "./sanitize.ts";
import { readOfficialLinkFromSettings } from "./settingsLink.ts";

/** 探针安全闸（红线，工作包 B 全局约束）：全部硬编码，运行期不可放宽。 */
export const PROBE_GUARDS = {
  /** 全程只允许存在一条上游连接；连接失败/被拒不重试（relayAuth 结构性无重连路径）。 */
  singleConnection: true,
  /** 无自动重连：任何 error/close 都是终态（官方终端有退避重连，探针一律不学）。 */
  reconnectPolicy: "never" as const,
  /** 收到 KICKED 立即退出并保持退出（不与用户官方页面互踢）。 */
  stopOnKicked: true,
  /**
   * 只读白名单：paired 后仅允许 bootstrap-request / workspace-list-request 两类纯读
   * 查询（运行期强制点 = appPayload.ts 的 READONLY_REQUEST_TYPES 闸）。
   */
  readonlyOnly: true,
  /**
   * connect 阶段硬预算（ms）：配对窗口 30s（官方同款 relayAuth.OFFICIAL_PAIR_WINDOW_MS）
   * + 2×10s 只读查询 = 50s < 60s。OAuth 回调等待单独有界（--wait-ms，默认 300s）。
   * 注意：这是**声明性预算，不是强制计时器**——本字段不设 wall-clock 强杀，探针整体
   * 不按它中断；时限由各阶段自有界承担（waitCallback / READ_QUERY_TIMEOUT_MS /
   * RELAY_HTTP_TIMEOUT_MS / relayAuth 配对窗口）。
   */
  maxDurationMs: 60_000,
  /** 凭据只经运行期输入（浏览器授权 / stdin），禁止写盘、禁止进环境变量与日志。 */
  credentialsInMemoryOnly: true,
} as const;

/** 唯一放行开关：真实连接必须由操作员显式提供，仓库内不得出现其真值。 */
const UNLOCK_FLAG = "--i-know-this-connects";

/** OAuth 回调默认等待窗口（有界；--wait-ms 可覆盖，上限 1 小时）。 */
const DEFAULT_OAUTH_WAIT_MS = 300_000;
/** 只读单查询超时：官方 payloadRequester 默认 10s（payloadRequester.ts:67）。 */
const READ_QUERY_TIMEOUT_MS = 10_000;
/** relay HTTP 请求（token 申请）单次超时。 */
const RELAY_HTTP_TIMEOUT_MS = 10_000;
/** JWT 形状闸：三段 base64url（只判形，不解码不校验签名——验签是 relay 的职责）。 */
const JWT_SHAPE = /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/;
/** 默认产品 DB 路径（只读打开）。 */
const DEFAULT_DB_PATH = "~/.tuvis/tuvis.db";

export type ProbeStep = {
  id: string;
  title: string;
  /** 该步骤的前置授权/输入（缺一即中止，不猜测）。 */
  requires: string;
  detail: string;
};

/** 探针步骤说明（--help 打印；与实现一一对应）。 */
export const PROBE_STEPS: readonly ProbeStep[] = [
  {
    id: "s1-acquire-link",
    title: "取当前有效官方链接",
    requires: "用户本人复制链接并经 A4 设置卡保存（或 --sid 现场指定）",
    detail:
      "只读打开 ~/.tuvis/tuvis.db 的 settings 读 remote.zcode_official_url，按 A4 合同复验并解析 sid/mid；" +
      "链接与 sid 只入内存 + 私有证据目录（别名化）。",
  },
  {
    id: "s2-account-jwt",
    title: "probe-oauth：账号凭据通路定案（interim→proven/unsupported 分岔点）",
    requires: "用户本人在浏览器完成官方 OAuth；Agent 不代登、不读浏览器/其他应用凭据",
    detail:
      "授权 URL 按 --provider 通道分派（zai 用官方公开 client_id；bigmodel 用 appId=zcode），" +
      "redirect_uri=http://127.0.0.1:<随机端口>/callback；" +
      "授权服务器接受回环 redirect 且 code 换 token 成功 → PROVEN；" +
      "authorize-error / 交换被拒 / 回调超窗 → 结构化 UNSUPPORTED（脱敏）。JWT 只存内存。",
  },
  {
    id: "s3-terminal-token",
    title: "申请 role=terminal 一次性 token（两次，验证一次性）",
    requires: "s2 取得 JWT + 官方链接 origin（relay 主机 = 同 origin 主机）",
    detail:
      'POST /api/v1/remote/token（Bearer JWT，{role:"terminal"}）；401→auth-required 即停；' +
      "两次申请的 token 值必须不同（官方一次性语义）；均只以掩码形态出现在输出/证据。",
  },
  {
    id: "s4-single-connect",
    title: "单条 WS 连接 + auth_init 只读握手",
    requires: "s3 的第一枚 token；用户确认此刻不依赖官方网页会话（同 role 连接会互踢）",
    detail:
      "wss://<官方链接主机>/api/v1/remote/ws2?token=<一次性>（链接 mid 非空时追加 &mid=，" +
      "官方 webRemoteControlTransport.ts:107-110）；auth_init→auth_ack；" +
      "waiting = 终端尚未可路由到设备（缺 mid/设备离线）：官方无 APP 侧配对确认，" +
      "30s 路由窗口自动超时；KICKED/AUTH_FAILED/路由窗口超时 → 终态即停（无重连）。",
  },
  {
    id: "s5-readonly-observe",
    title: "只读查询设备/workspace/session 状态",
    requires: "s4 进入 paired",
    detail:
      "只发白名单内两个纯读查询：bootstrap-request（窗口/版本/workspace 列表/活动态）→ " +
      "workspace-list-request（workspace 列表 + 活动键）；不应答交互、不开工作区桥、不改状态；" +
      "响应按字段类别别名化后入私有证据。",
  },
  {
    id: "s6-teardown",
    title: "断开与证据落盘",
    requires: "无",
    detail:
      "主动 close；脱敏 transcript.json + run.md 写入 ~/Documents/tuvis-verify-logs/zcode/ 私有运行目录" +
      "（0600，fail-closed：含真实值的内容拒写）；控制台只打印私有路径与掩码值。",
  },
];

type ProbeFlags = {
  unlock: boolean;
  open: boolean;
  oauthWaitMs: number;
  jwtStdin: boolean;
  sid: string | null;
  dbPath: string | null;
  provider: OAuthProviderChannel;
  help: boolean;
};

function parseFlags(argv: readonly string[]): ProbeFlags | string {
  const flags: ProbeFlags = {
    unlock: false,
    open: false,
    oauthWaitMs: DEFAULT_OAUTH_WAIT_MS,
    jwtStdin: false,
    sid: null,
    dbPath: null,
    provider: "zai",
    help: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--i-know-this-connects") flags.unlock = true;
    else if (arg === "--open") flags.open = true;
    else if (arg === "--jwt-stdin") flags.jwtStdin = true;
    else if (arg === "--help" || arg === "-h") flags.help = true;
    else if (arg === "--wait-ms" || arg.startsWith("--wait-ms=")) {
      const raw = arg === "--wait-ms" ? argv[i + 1] : arg.slice("--wait-ms=".length);
      if (raw === undefined) return "--wait-ms 需要一个毫秒值";
      const value = Number(raw);
      if (!Number.isFinite(value) || value <= 0 || value > 3_600_000) {
        return "--wait-ms 必须是 (0, 3600000] 内的毫秒数";
      }
      flags.oauthWaitMs = value;
      if (arg === "--wait-ms") i += 1;
    } else if (arg.startsWith("--provider=")) {
      // 通道字面量与官方 provider id 一致（oauth.ts:9,12）；非通道值一律拒绝，不猜。
      const value = arg.slice("--provider=".length).trim();
      if (value === "zai" || value === "bigmodel") {
        flags.provider = value;
      } else {
        return "--provider 只接受 zai | bigmodel";
      }
    } else if (arg === "--provider") {
      return "--provider 需要 =<值> 形式，例如 --provider=bigmodel";
    } else if (arg.startsWith("--sid=")) {
      const value = arg.slice("--sid=".length).trim();
      const bytes = new TextEncoder().encode(value).length;
      if (!value || bytes > 128) return "--sid 必须是非空且 ≤128 字节的字符串";
      flags.sid = value;
    } else if (arg.startsWith("--db=")) {
      const value = arg.slice("--db=".length).trim();
      if (!value) return "--db 需要一个 SQLite 路径";
      flags.dbPath = value;
    } else {
      return `未知参数：${arg}`;
    }
  }
  return flags;
}

function printHelp(): void {
  const lines: string[] = [];
  lines.push("zcode-bridge-probe / auth-check — B1 官方桥接实连探针（只读 + 授权通路定案）");
  lines.push("");
  lines.push("用法：node scripts/zcode-bridge-probe/auth-check.ts <flags>");
  lines.push("");
  lines.push("安全闸（硬编码，不可放宽）：");
  lines.push(`  ${JSON.stringify(PROBE_GUARDS)}`);
  lines.push("");
  lines.push("参数：");
  lines.push(`  ${UNLOCK_FLAG}                 必需。显式确认「我知道这会发起真实连接」；`);
  lines.push("                             缺席时本工具只打印本帮助并拒绝执行。");
  lines.push("  --open                    授权阶段用系统默认浏览器打开授权 URL（默认只打印）。");
  lines.push("  --wait-ms=<ms>            OAuth 回调等待窗口（默认 300000，上限 3600000，有界）。");
  lines.push("  --jwt-stdin               重跑模式：跳过 OAuth，从 stdin 读账号 JWT（只入内存）。");
  lines.push(
    "  --provider=<zai|bigmodel> OAuth 通道（默认 zai）：zai = 国际站 chat.z.ai 授权入口；"
  );
  lines.push(
    "                             bigmodel = 国内智谱 bigmodel.cn 授权入口（appId=zcode）。"
  );
  lines.push("                             两通道共用同一交换端点与下游 relay（s3—s6 不受影响），");
  lines.push("                             仅授权 URL 形状与交换 body 的 provider 字面量不同。");
  lines.push(
    "  --sid=<device_sid>        跳过 DB 读取，直接指定 device_sid（origin 取官方默认）。"
  );
  lines.push(
    "                            注意：命令行参数在本机 ps 进程列表里可见，建议优先走默认 DB 路径。"
  );
  lines.push(`  --db=<path>               settings 库路径（默认 ${DEFAULT_DB_PATH}，只读打开）。`);
  lines.push("  --help                    打印本帮助。");
  lines.push("");
  lines.push("步骤：");
  for (const step of PROBE_STEPS) {
    lines.push(`  [${step.id}] ${step.title}`);
    lines.push(`      前置：${step.requires}`);
    lines.push(`      ${step.detail}`);
  }
  lines.push("");
  lines.push("用户交互：");
  lines.push("  · 授权由你本人在浏览器完成（探针不碰密码、不代登）；");
  lines.push("  · waiting = 终端尚未可路由到设备（缺 mid/设备离线）：官方无 APP 侧配对确认，");
  lines.push("    30 秒路由窗口自动超时（无需也不会有任何 APP 内操作）；");
  lines.push(
    "  · 探针连接可能踢掉你正在用的官方网页会话（同 role 单 owner），请在此时段不依赖网页端。"
  );
  lines.push("");
  lines.push(
    `证据：~/Documents/tuvis-verify-logs/zcode/macos-B1<UTC 时间戳>-<随机码>/（0600，脱敏）。`
  );
  console.log(lines.join("\n"));
}

/** 控制台输出统一加前缀；调用方保证内容已脱敏（授权 URL 除外——用户点击必需）。 */
function log(line: string): void {
  console.log(`[b1-probe] ${line}`);
}

async function readAllStdin(): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) {
    chunks.push(chunk as Buffer);
  }
  return Buffer.concat(chunks).toString("utf8");
}

function openInBrowser(url: string): boolean {
  const platform = process.platform;
  const command = platform === "darwin" ? "open" : platform === "win32" ? "cmd" : "xdg-open";
  const args = platform === "win32" ? ["/c", "start", "", url] : [url];
  try {
    const child = childProcess.spawn(command, args, { detached: true, stdio: "ignore" });
    child.unref();
    return true;
  } catch {
    return false;
  }
}

/** relay WS URL 派生：与端点 origin 同主机、协议换 wss、路径固定（zcodeEndpoint.ts:369-380）。 */
function relayWsUrlFromOrigin(origin: string): string {
  const parsed = new URL(origin);
  const protocol = parsed.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${parsed.host}${RELAY_WS_PATH}`;
}

/** 读响应里出现的身份/位置字段 → 登记别名（脱敏后入证据）。字段表按官方 payload 形状。 */
const SENSITIVE_STRING_FIELDS: Array<{
  key: string;
  kind: Parameters<SecretRedactor["register"]>[0];
}> = [
  { key: "sid", kind: "sid" },
  { key: "mid", kind: "mid" },
  { key: "workspacePath", kind: "path" },
  { key: "workspaceIdentity", kind: "sid" },
  { key: "remoteSessionId", kind: "sid" },
  { key: "workspaceKey", kind: "sid" },
  { key: "windowControlSessionId", kind: "sid" },
  { key: "terminalSid", kind: "sid" },
  { key: "deviceSid", kind: "sid" },
  { key: "deviceMid", kind: "mid" },
  { key: "label", kind: "name" },
  { key: "name", kind: "name" },
  { key: "taskId", kind: "sid" },
  { key: "taskHostId", kind: "sid" },
  { key: "projectId", kind: "sid" },
  { key: "initialTaskId", kind: "sid" },
  { key: "activeWorkspaceKey", kind: "sid" },
  { key: "activeTaskId", kind: "sid" },
  { key: "bridgeSessionId", kind: "sid" },
  { key: "bridgeToken", kind: "relay-token" },
  { key: "deviceToken", kind: "relay-token" },
  { key: "token", kind: "jwt" },
  { key: "connectUrl", kind: "link" },
  { key: "qrUrl", kind: "link" },
  { key: "wsUrl", kind: "link" },
];

/**
 * 键名正则网（exact 名单之外的兜底，评审 Important）：入站推送帧/只读响应里的键名
 * 若未命中 exact 名单但命中本网，一律按敏感处理（宁过勿漏）——评审示例
 * `sessionId`（session）/ `access_token`（token）由此覆盖，`secret`/`jwt`/`url`/`path`
 * 同网。合法非敏感键（platform / client_ts / zcode_type / requestId / success …）
 * 均不命中本网，照常透传。命中键的字符串值经 register 登记别名，assertClean 对
 * 登记值 fail-closed（真实值再入证据即拒写）。
 */
const SENSITIVE_KEY_NET = /(?:sid|session|token|secret|jwt|url|path)/i;

/** 正则网命中键 → 别名类别（推断序固定：先凭据类后位置类；余下一律按 sid）。 */
function netSecretKind(key: string): Parameters<SecretRedactor["register"]>[0] {
  if (/jwt/i.test(key)) return "jwt";
  if (/token|secret/i.test(key)) return "relay-token";
  if (/url/i.test(key)) return "link";
  if (/path/i.test(key)) return "path";
  return "sid";
}

/**
 * 就地登记 payload 内敏感字段并为脱敏副本返回同结构对象（值已别名化）：
 * 先按 exact 名单（键名精确命中，类别显式），再按 SENSITIVE_KEY_NET 兜底。
 * 仅字符串值参与登记（数字/布尔等非凭据形态原样透传）。
 */
export function redactPayload(redactor: SecretRedactor, payload: unknown): unknown {
  const walk = (value: unknown): unknown => {
    if (Array.isArray(value)) return value.map(walk);
    if (!value || typeof value !== "object") return value;
    const out: Record<string, unknown> = {};
    for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
      const field = SENSITIVE_STRING_FIELDS.find((candidate) => candidate.key === key);
      if (field && typeof item === "string" && item) {
        out[key] = redactor.register(field.kind, item);
        continue;
      }
      if (!field && typeof item === "string" && item && SENSITIVE_KEY_NET.test(key)) {
        out[key] = redactor.register(netSecretKind(key), item);
        continue;
      }
      out[key] = walk(item);
    }
    return out;
  };
  return walk(payload);
}

/** 只读查询结果摘要（脱敏前的实内存对象由 redactPayload 处理）。 */
function summarizeReadOutcome(
  redactor: SecretRedactor,
  type: string,
  outcome: { kind: string; payload?: Record<string, unknown> }
): Record<string, unknown> {
  if (outcome.kind !== "ok") return { request: type, outcome: outcome.kind };
  const payload = outcome.payload as {
    success?: unknown;
    result?: unknown;
    error?: unknown;
  };
  return {
    request: type,
    outcome: "ok",
    success: payload.success === true,
    error: typeof payload.error === "string" ? redactor.sanitizeText(payload.error) : null,
    result: redactPayload(redactor, payload.result ?? null),
  };
}

type ProbeOutcome = {
  oauthVerdict: string;
  connectVerdict: string;
  exitCode: number;
};

/** 探针主体。返回退出码：0=完成并落证（含 UNSUPPORTED/STOPPED 等明确结论），2=前置/用法错误。 */
export async function runProbe(argv: readonly string[]): Promise<ProbeOutcome> {
  const parsed = parseFlags(argv);
  if (typeof parsed === "string") {
    console.error(`[b1-probe] 参数错误：${parsed}`);
    printHelp();
    return { oauthVerdict: "NOT-RUN", connectVerdict: "NOT-RUN", exitCode: 2 };
  }
  if (parsed.help) {
    printHelp();
    return { oauthVerdict: "NOT-RUN", connectVerdict: "NOT-RUN", exitCode: 0 };
  }
  if (!parsed.unlock) {
    printHelp();
    console.error(
      `\n[b1-probe] 拒绝执行：缺少 ${UNLOCK_FLAG}。本探针会发起真实网络连接 ` +
        `(chat.z.ai / bigmodel.cn / zcode.z.ai / 本地回环回调)，必须由操作员显式解锁。`
    );
    return { oauthVerdict: "NOT-RUN", connectVerdict: "NOT-RUN", exitCode: 2 };
  }

  const redactor = new SecretRedactor();
  const run = createEvidenceRunDir();
  const events: Array<Record<string, unknown>> = [];
  const startedIso = new Date().toISOString();
  const record = (step: string, message: string, data?: unknown): void => {
    events.push({
      t: new Date().toISOString(),
      step,
      message,
      ...(data !== undefined ? { data } : {}),
    });
  };
  log(`证据目录（私有，勿提交/外发）：${run.dir}`);

  let jwt: string | null = null;
  let oauthVerdict = "SKIPPED";
  // provider 通道字面量不是秘密（oauth.ts:9,12），如实入 summary/证据。
  const oauthSummary: Record<string, unknown> = {
    attempted: !parsed.jwtStdin,
    provider: parsed.provider,
  };

  // —— Step 2（s2）：probe-oauth ——————————————————————————————————————————
  if (!parsed.jwtStdin) {
    const isZai = parsed.provider === "zai";
    record("probe-oauth", "start", {
      provider: parsed.provider,
      authorizeOrigin: isZai ? OFFICIAL_AUTHORIZE_ORIGIN : OFFICIAL_BIGMODEL_AUTHORIZE_ORIGIN,
      authorizePath: isZai ? OFFICIAL_AUTHORIZE_PATH : OFFICIAL_BIGMODEL_AUTHORIZE_PATH,
      clientIdentifierField: isZai ? "client_id" : "appId",
      clientIdentifier: isZai ? OFFICIAL_OAUTH_CLIENT_ID : OFFICIAL_BIGMODEL_APP_ID,
      testTarget: "redirect_uri = http://127.0.0.1:<random-port>/callback",
    });
    const state = generateOAuthState();
    redactor.register("state", state);
    const server = await LoopbackCallbackServer.start();
    // 监听器生命周期受 try/finally 保护（评审 Minor）：URL 构造/打印/等待回调中途抛错
    // 也必须 close()，绝不让回环监听器留在事件循环里挂着（进程无法退出）。
    let callback: LoopbackCallback;
    let redirectUri: string;
    try {
      redirectUri = server.redirectUri;
      const authorizeUrl = buildAuthorizeUrl(parsed.provider, { redirectUri, state });
      // 完整授权 URL 也登记为 link 类：任何意外把它带进证据都会被别名化。
      redactor.register("link", authorizeUrl);
      oauthSummary.redirect = { host: "127.0.0.1", port: server.port, path: "/callback" };
      oauthSummary.state = redactor.register("state", state);
      record("probe-oauth", "listening", {
        redirectHost: "127.0.0.1",
        redirectPort: server.port,
        state: "STATE_1",
      });
      log(
        parsed.provider === "bigmodel"
          ? "请在浏览器打开下面的官方授权 URL，并用你的国内智谱（bigmodel.cn）账号完成授权（探针绝不代登）："
          : "请在浏览器打开下面的官方授权 URL，并用你的 ZCode 账号完成授权（探针绝不代登）："
      );
      // 该行故意打印到 stdout 供用户点击；state/link 已登记 redactor，落入证据文件时
      // 只会是别名。stdout 不属于证据落盘面，也不写仓库/日志文件。
      log(`  ${authorizeUrl}`);
      if (parsed.open) {
        log(
          openInBrowser(authorizeUrl)
            ? "已请求系统默认浏览器打开该 URL（--open）。"
            : "--open 打开浏览器失败，请手动复制上面的 URL。"
        );
      }
      log(`等待回环回调（有界 ${parsed.oauthWaitMs} ms）……`);
      callback = await server.waitCallback(state, parsed.oauthWaitMs);
    } finally {
      await server.close();
    }
    record("probe-oauth", "wait-finished", {
      kind: callback.kind,
      ignored: server.ignoredRequests,
    });

    if (callback.kind === "code") {
      redactor.register("auth-code", callback.code);
      record("probe-oauth", "callback-code", { code: "CODE_1", masked: maskOpaque(callback.code) });
      const exchange = await exchangeCodeForToken({
        zcodeOrigin: OFFICIAL_ZCODE_ENDPOINT_ORIGIN,
        code: callback.code,
        redirectUri,
        state,
        provider: parsed.provider,
      });
      if (exchange.kind === "ok") {
        jwt = exchange.jwt;
        redactor.register("jwt", jwt);
        oauthVerdict = "PROVEN";
        oauthSummary.verdict = "PROVEN";
        oauthSummary.jwt = {
          alias: "JWT_1",
          masked: maskJwt(jwt),
          expiresInSeconds: exchange.expiresInSeconds,
          hasUserInfo: exchange.hasUserInfo,
        };
        record("probe-oauth", "exchange-ok", {
          verdict: "PROVEN",
          jwt: "JWT_1",
          masked: maskJwt(jwt),
          expiresInSeconds: exchange.expiresInSeconds,
        });
        log(`probe-oauth verdict = PROVEN —— 官方授权服务器接受了 Tuvis 回环 redirect。`);
        log(`  账号 JWT ${maskJwt(jwt)}（仅存内存，不落盘；auth.status 可定案 proven）。`);
      } else if (exchange.kind === "rejected") {
        oauthVerdict = "UNSUPPORTED";
        oauthSummary.verdict = "UNSUPPORTED";
        oauthSummary.stage = "token-exchange";
        oauthSummary.reason = exchange.reason;
        oauthSummary.httpStatus = exchange.httpStatus ?? null;
        oauthSummary.serverCode = exchange.serverCode ?? null;
        oauthSummary.serverMsg = exchange.serverMsg
          ? redactor.sanitizeText(exchange.serverMsg)
          : null;
        record("probe-oauth", "exchange-rejected", {
          verdict: "UNSUPPORTED",
          reason: exchange.reason,
          httpStatus: exchange.httpStatus ?? null,
          serverCode: exchange.serverCode ?? null,
        });
        log(
          `probe-oauth verdict = UNSUPPORTED —— 授权服务器接受了 redirect，但 code 换 token 被拒 ` +
            `(${exchange.reason}${exchange.httpStatus ? ` http=${exchange.httpStatus}` : ""}${
              exchange.serverCode !== null && exchange.serverCode !== undefined
                ? ` code=${exchange.serverCode}`
                : ""
            })。`
        );
      } else {
        oauthVerdict = "INCONCLUSIVE";
        oauthSummary.verdict = "INCONCLUSIVE";
        oauthSummary.stage = "token-exchange";
        oauthSummary.reason = "unreachable";
        record("probe-oauth", "exchange-unreachable", { verdict: "INCONCLUSIVE" });
        log(
          "probe-oauth verdict = INCONCLUSIVE —— 交换端点不可达（网络/服务故障），不构成 unsupported 证据。"
        );
      }
    } else if (callback.kind === "authorize-error") {
      oauthVerdict = "UNSUPPORTED";
      oauthSummary.verdict = "UNSUPPORTED";
      oauthSummary.stage = "authorize";
      oauthSummary.error = callback.error;
      oauthSummary.errorDescription = callback.errorDescription
        ? redactor.sanitizeText(callback.errorDescription)
        : null;
      record("probe-oauth", "authorize-error", {
        verdict: "UNSUPPORTED",
        error: callback.error,
        stateMatched: callback.state === state,
      });
      log(
        `probe-oauth verdict = UNSUPPORTED —— 官方授权服务器在授权页直接拒绝 ` +
          `(error=${callback.error})：回环 redirect_uri 未被接受。`
      );
    } else {
      oauthVerdict = "UNSUPPORTED";
      oauthSummary.verdict = "UNSUPPORTED";
      oauthSummary.stage = "callback-wait";
      oauthSummary.timeoutMs = parsed.oauthWaitMs;
      oauthSummary.note =
        "timeout ≠ 服务器拒绝证据：只说明窗口内未收到回环回调（用户未完成/未发起即属此列）；" +
        "定案 unsupported 需结合 authorize-error 或交换被拒。";
      record("probe-oauth", "callback-timeout", {
        verdict: "UNSUPPORTED",
        timeoutMs: parsed.oauthWaitMs,
      });
      log(
        `probe-oauth verdict = UNSUPPORTED（callback-timeout，${parsed.oauthWaitMs} ms 窗口内未收到回调）。`
      );
    }
  } else {
    record("probe-oauth", "skipped", { reason: "jwt-stdin rerun" });
    log("probe-oauth 跳过（--jwt-stdin 重跑模式）。");
  }

  // —— s3—s6：connect-readonly ————————————————————————————————————————————
  let connectVerdict = "SKIPPED";
  const connectSummary: Record<string, unknown> = { attempted: false };

  if (parsed.jwtStdin) {
    const raw = (await readAllStdin()).trim();
    if (!JWT_SHAPE.test(raw)) {
      record("connect-readonly", "jwt-invalid-shape", {});
      console.error("[b1-probe] stdin 内容不是 JWT 形状（三段 base64url），拒绝执行。");
      await writeEvidence(
        redactor,
        run,
        startedIso,
        parsed,
        oauthVerdict,
        oauthSummary,
        "ABORTED:jwt-shape",
        {},
        events
      );
      return { oauthVerdict, connectVerdict: "ABORTED:jwt-shape", exitCode: 2 };
    }
    jwt = raw;
    redactor.register("jwt", jwt);
    connectSummary.jwt = { alias: "JWT_1", masked: maskJwt(jwt) };
    record("connect-readonly", "jwt-from-stdin", { jwt: "JWT_1", masked: maskJwt(jwt) });
  }

  if (jwt) {
    connectSummary.attempted = true;
    // s1：sid/origin/mid——mid 与 sid 同出自官方链接（MID_A），随 s4 透传进 ws2 升级 URL
    // （官方条件性 webRemoteControlTransport.ts:107-110）；--sid 直指定时无 mid（null）。
    let sid: string;
    let mid: string | null = null;
    let origin = OFFICIAL_ZCODE_ENDPOINT_ORIGIN;
    if (parsed.sid) {
      sid = parsed.sid;
      redactor.register("sid", sid);
      record("connect-readonly", "sid-from-cli", { sid: "SID_A" });
      connectSummary.linkSource = "cli";
    } else {
      const dbPath = parsed.dbPath ?? path.join(os.homedir(), ".tuvis", "tuvis.db");
      connectSummary.dbPath = dbPath;
      const settings = readOfficialLinkFromSettings(dbPath);
      if (settings.kind !== "ok") {
        connectVerdict = `ABORTED:settings-${settings.kind}`;
        connectSummary.verdict = connectVerdict;
        record("connect-readonly", "settings-read-failed", { kind: settings.kind });
        log(
          `connect-readonly 中止：无法从 ${dbPath} 读取官方链接（${settings.kind}${
            settings.kind === "unavailable" ? `: ${settings.reason}` : ""
          }）。请先经 Tuvis A4 设置卡保存有效链接，或用 --sid=… 指定。`
        );
        await writeEvidence(
          redactor,
          run,
          startedIso,
          parsed,
          oauthVerdict,
          oauthSummary,
          connectVerdict,
          connectSummary,
          events
        );
        return { oauthVerdict, connectVerdict, exitCode: 2 };
      }
      const parsedLink = parseOfficialLink(settings.link);
      if (!parsedLink.ok) {
        connectVerdict = `ABORTED:link-invalid-${parsedLink.reason}`;
        connectSummary.verdict = connectVerdict;
        record("connect-readonly", "link-invalid", { reason: parsedLink.reason });
        log(`connect-readonly 中止：已保存链接未通过 A4 复验（${parsedLink.reason}）。`);
        await writeEvidence(
          redactor,
          run,
          startedIso,
          parsed,
          oauthVerdict,
          oauthSummary,
          connectVerdict,
          connectSummary,
          events
        );
        return { oauthVerdict, connectVerdict, exitCode: 2 };
      }
      redactor.register("link", settings.link);
      sid = parsedLink.sid;
      mid = parsedLink.mid;
      origin = parsedLink.origin;
      redactor.register("sid", sid);
      redactor.register("mid", parsedLink.mid);
      if (parsedLink.name) redactor.register("name", parsedLink.name);
      connectSummary.linkSource = "settings";
      connectSummary.link = {
        alias: "LINK_1",
        origin: parsedLink.origin,
        pathVersion: parsedLink.pathVersion,
        sid: "SID_A",
        mid: parsedLink.mid ? "MID_A" : null,
        appVersion: parsedLink.appVersion,
      };
      record("connect-readonly", "link-loaded", connectSummary.link);
    }

    // s3：一次性 token ×2
    const relayWsUrl = relayWsUrlFromOrigin(origin);
    connectSummary.relay = {
      origin,
      tokenUrl: new URL("/api/v1/remote/token", origin).toString(),
      wsUrl: relayWsUrl,
      // mid 是否随 ws2 升级 URL 透传（官方条件性 webRemoteControlTransport.ts:107-110）——
      // f1c350a4 及之前恒缺 mid → relay 无法路由 → pair_status 恒 waiting → pair-timeout。
      midAttached: mid !== null,
      derivation: "relay host = 官方链接 origin 同主机（zcodeEndpoint.ts:369-380,14）",
    };
    log(`relay 派生：${origin} → ${relayWsUrl}`);
    const tokenFirst = await requestTerminalToken({
      relayOrigin: origin,
      accountJwt: jwt,
      signal: AbortSignal.timeout(RELAY_HTTP_TIMEOUT_MS),
    });
    if (tokenFirst.kind !== "ok") {
      connectVerdict = `STOPPED:token-${tokenFirst.kind}`;
      connectSummary.verdict = connectVerdict;
      record("connect-readonly", "token-request-failed", { kind: tokenFirst.kind });
      log(
        tokenFirst.kind === "auth-required"
          ? "connect-readonly 停止：relay 以 401 拒绝了账号 JWT（auth-required）——JWT 无效/过期，不重试。"
          : "connect-readonly 停止：relay token 端点不可用/响应形状不符（relay-unavailable），不重试。"
      );
      await writeEvidence(
        redactor,
        run,
        startedIso,
        parsed,
        oauthVerdict,
        oauthSummary,
        connectVerdict,
        connectSummary,
        events
      );
      return { oauthVerdict, connectVerdict, exitCode: 0 };
    }
    redactor.register("relay-token", tokenFirst.token);
    const tokenSecond = await requestTerminalToken({
      relayOrigin: origin,
      accountJwt: jwt,
      signal: AbortSignal.timeout(RELAY_HTTP_TIMEOUT_MS),
    });
    const oneTimeDifferent =
      tokenSecond.kind === "ok" ? tokenSecond.token !== tokenFirst.token : null;
    if (tokenSecond.kind === "ok") redactor.register("relay-token", tokenSecond.token);
    connectSummary.terminalToken = {
      first: {
        alias: "TOK_1",
        masked: maskOpaque(tokenFirst.token),
        expiresInSeconds: tokenFirst.expiresInSeconds,
      },
      second:
        tokenSecond.kind === "ok"
          ? {
              alias: "TOK_2",
              masked: maskOpaque(tokenSecond.token),
              expiresInSeconds: tokenSecond.expiresInSeconds,
            }
          : { kind: tokenSecond.kind },
      oneTimeDifferent,
    };
    record("connect-readonly", "terminal-token", {
      first: "TOK_1",
      second: tokenSecond.kind === "ok" ? "TOK_2" : tokenSecond.kind,
      oneTimeDifferent,
      expiresInSeconds: tokenFirst.expiresInSeconds,
      tokenShape: "22-char base64url, last char in [AQgw]（relayAuth OFFICIAL_TOKEN_SHAPE 已验）",
    });
    log(
      `terminal token OK：expires_in=${tokenFirst.expiresInSeconds}s；一次性核验（两次申请值不同）= ${oneTimeDifferent}。`
    );

    // s4：单条连接 + 握手
    log("建立单条 WS 连接（auth_init → auth_ack）……");
    log(
      "waiting = 终端尚未可路由到设备（缺 mid/设备离线）；官方无 APP 侧配对确认，" +
        "30 秒路由窗口自动超时；期间请勿依赖官方网页会话（同 role 连接会互踢）。"
    );
    const conn = new RelayTerminalConnection({
      relayWsUrl,
      token: tokenFirst.token,
      deviceSid: sid,
      deviceMid: mid,
    });
    const channel = new ReadonlyPayloadChannel();
    channel.attach(conn);
    const outcome = await conn.connect();
    if (outcome !== "paired") {
      connectVerdict = `STOPPED:${outcome}`;
      connectSummary.verdict = connectVerdict;
      record("connect-readonly", "handshake-terminal", { outcome });
      log(`connect-readonly 停止：握手终态 = ${outcome}（无重试，立即收尾）。`);
      conn.close();
      channel.detach();
      for (const frame of channel.unsolicited) {
        record("unsolicited-frame", "data", redactPayload(redactor, frame));
      }
      await writeEvidence(
        redactor,
        run,
        startedIso,
        parsed,
        oauthVerdict,
        oauthSummary,
        connectVerdict,
        connectSummary,
        events
      );
      return { oauthVerdict, connectVerdict, exitCode: 0 };
    }
    record("connect-readonly", "paired", {});
    log("已配对（pair_status=matched）。发送只读查询（白名单内仅两个）……");

    // s5：只读查询（bootstrap → workspace-list，官方终端同款两个纯读请求）
    const bootstrap = await channel.request(conn, "bootstrap-request", READ_QUERY_TIMEOUT_MS);
    const bootstrapSummary = summarizeReadOutcome(redactor, "bootstrap-request", bootstrap);
    record("readonly-query", "bootstrap-request", bootstrapSummary);
    log(`bootstrap-request → ${bootstrap.kind}`);
    const workspaceList = await channel.request(
      conn,
      "workspace-list-request",
      READ_QUERY_TIMEOUT_MS
    );
    const listSummary = summarizeReadOutcome(redactor, "workspace-list-request", workspaceList);
    record("readonly-query", "workspace-list-request", listSummary);
    log(`workspace-list-request → ${workspaceList.kind}`);

    connectVerdict =
      bootstrap.kind === "ok" && workspaceList.kind === "ok" ? "COMPLETED" : "COMPLETED-PARTIAL";
    connectSummary.verdict = connectVerdict;
    connectSummary.reads = { bootstrap: bootstrap.kind, workspaceList: workspaceList.kind };

    // s6：断开与落证
    conn.close();
    channel.detach();
    for (const frame of channel.unsolicited) {
      record("unsolicited-frame", "data", redactPayload(redactor, frame));
    }
    record("connect-readonly", "closed", {});
    log(
      "连接已主动断开（探针侧 close；无重连路径）。JWT/一次性 token 只存在于进程内存，随进程退出即弃。"
    );
  } else {
    connectSummary.verdict = "SKIPPED";
    connectSummary.reason = "no-jwt-in-process";
    record("connect-readonly", "skipped", { reason: "no-jwt-in-process" });
    log("connect-readonly 跳过：进程内没有可用的账号 JWT（probe-oauth 未取得 PROVEN）。");
  }

  await writeEvidence(
    redactor,
    run,
    startedIso,
    parsed,
    oauthVerdict,
    oauthSummary,
    connectVerdict,
    connectSummary,
    events
  );
  return { oauthVerdict, connectVerdict, exitCode: 0 };
}

/** 组装并 fail-closed 落盘脱敏证据；控制台只打印私有路径。 */
async function writeEvidence(
  redactor: SecretRedactor,
  run: { dir: string; runId: string },
  startedIso: string,
  flags: ProbeFlags,
  oauthVerdict: string,
  oauthSummary: Record<string, unknown>,
  connectVerdict: string,
  connectSummary: Record<string, unknown>,
  events: Array<Record<string, unknown>>
): Promise<void> {
  const finishedIso = new Date().toISOString();
  const transcript = {
    runId: run.runId,
    probe: "zcode-bridge-auth-check",
    workPackage: "B1 实连半场：官方授权通路定案 + 只读连接探针",
    sourceBaseline: "zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab",
    startedUtc: startedIso,
    finishedUtc: finishedIso,
    guards: PROBE_GUARDS,
    flags: {
      provider: flags.provider,
      unlock: flags.unlock,
      openRequested: flags.open,
      oauthWaitMs: flags.oauthWaitMs,
      jwtViaStdin: flags.jwtStdin,
      sidFromCli: flags.sid !== null,
      dbPath: flags.dbPath,
    },
    oauth: oauthSummary,
    connect: connectSummary,
    events,
  };
  const transcriptJson = JSON.stringify(redactor.sanitizeDeep(transcript), null, 2);
  const runMd = [
    `# B1 实连半场探针 run — ${run.runId}`,
    "",
    `- started: ${startedIso}`,
    `- finished: ${finishedIso}`,
    `- oauth verdict: **${oauthVerdict}**`,
    `- connect verdict: **${connectVerdict}**`,
    `- guards: \`${JSON.stringify(PROBE_GUARDS)}\``,
    "",
    "本目录为私有证据（不进仓库）。所有 SID/JWT/token/state/授权码/链接均以稳定别名",
    "（SID_A / JWT_1 / TOK_1 / STATE_1 / CODE_1 / LINK_1）与掩码形态记录；落盘前经",
    "fail-closed 断言（含真实值的内容拒写）。原始帧/真实值从未离开本进程内存。",
    "",
    "细节见同目录 transcript.json。",
    "",
  ].join("\n");
  writeEvidenceFiles(redactor, run.dir, [
    { name: "transcript.json", content: transcriptJson },
    { name: "run.md", content: runMd },
  ]);
  log(`证据已写入（脱敏）：${run.dir}`);
  log(`oauth=${oauthVerdict} connect=${connectVerdict}`);
}

/** 入口：只在被直接执行时运行（被 import 时不产生副作用）。 */
export async function main(argv: readonly string[] = process.argv.slice(2)): Promise<void> {
  try {
    const outcome = await runProbe(argv);
    process.exitCode = outcome.exitCode;
  } catch (error) {
    // 错误只打印 message（探针各模块不把凭据放进错误文案）。
    console.error(
      `[b1-probe] 探针异常终止：${error instanceof Error ? error.message : String(error)}`
    );
    process.exitCode = 1;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  void main();
}

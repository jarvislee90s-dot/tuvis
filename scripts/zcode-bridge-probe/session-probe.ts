// scripts/zcode-bridge-probe/session-probe.ts — ZCode 官方桥接「会话映射与单次发送」探针
// （B2 实连半场，controller 与用户在场交互执行；本仓库开发过程不代跑）。
//
// 归属：工作包 B 探针工具目录（不在产品运行链上：不被 src/ 引用、不挂应用启动/扫描）。
// 流程（全部有界、单连接、KICKED 即停、无重连）：
//   1. probe-oauth     —— 账号 JWT（默认 bigmodel 通道，B1 定案 proven；JWT 只存内存）；
//   2. connect         —— 官方链接 sid/mid → role=terminal 一次性 token → ws2?token&mid
//                         单条连接 auth_init→auth_ack→paired（SingleConnectionGate 全程
//                         只放行这一条）；
//   3. mapping         —— paired 后只发两个白名单只读查询 bootstrap-request /
//                         workspace-list-request（B1 已实连验证），并被动收帧组装
//                         controller/tasks-index 等 rpc-frame（B1 实连已观察到该推送），
//                         得到 workspace 条目 + task 行（精确 taskId/workspacePath）；
//   4. designate       —— 操作员指称（--workspace 默认 Travel、--task-title 默认 hi）→
//                         在 listing 里**精确唯一**匹配出真实 id；匹配不唯一/不命中一律
//                         中止（绝不按标题发送）；映射只绑定当前连接代际；
//   5. send（--send-authorized 显式解锁，缺省只打印 WOULD-send 即停）——
//                         workspace-bridge-open{workspaceKey, taskId} → ready →
//                         rpc-frame 通道 hello/clientHello →
//                         subscribeConversationV4{workspacePath, workspaceIdentity?,
//                         sessionId=<精确 taskId>}（订阅只为观察活动帧；官方发送只依赖
//                         握手）→ sendConversationCommandV4（sendText，正文 = 唯一合成
//                         marker `[tuvis-b2-marker:<UTC>]`，模型/模式全缺省=沿用会话现
//                         值）→ 记录受理/完成/未知全生命周期（恰一个 rpc-frame，重复去
//                         重，未知不重发）→ 主动 close。
//   证据：脱敏 transcript.json + run.md → ~/Documents/tuvis-verify-logs/zcode/
//   macos-B2<UTC 时间戳>-<随机码>/（0700 目录 / 0600 文件；fail-closed）。
//
// wire 依据 = 固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// （逐条行号见 sessionBridge.ts / relayAuth.ts 头注与决策文件 §6—§7）。
//
// 运行方式：`node scripts/zcode-bridge-probe/session-probe.ts <flags>`（Node ≥22.22
// type-stripping 直接执行；本目录只用可擦除 TS 语法、内置依赖，无 tsx/新依赖）。

import * as childProcess from "node:child_process";
import * as crypto from "node:crypto";
import * as os from "node:os";
import * as path from "node:path";
import { pathToFileURL } from "node:url";
import { READONLY_REQUEST_TYPES } from "./appPayload.ts";
import { redactPayload } from "./auth-check.ts";
import { parseOfficialLink } from "./officialLink.ts";
import {
  LoopbackCallbackServer,
  OFFICIAL_AUTHORIZE_ORIGIN,
  OFFICIAL_AUTHORIZE_PATH,
  OFFICIAL_BIGMODEL_APP_ID,
  OFFICIAL_BIGMODEL_AUTHORIZE_ORIGIN,
  OFFICIAL_BIGMODEL_AUTHORIZE_PATH,
  OFFICIAL_OAUTH_CLIENT_ID,
  OFFICIAL_ZCODE_ENDPOINT_ORIGIN,
  buildAuthorizeUrl,
  exchangeCodeForToken,
  generateOAuthState,
  type LoopbackCallback,
  type OAuthProviderChannel,
} from "./oauthFlow.ts";
import { RELAY_WS_PATH, RelayTerminalConnection, requestTerminalToken } from "./relayAuth.ts";
import {
  AGENT_CHANNEL,
  AGENT_CLIENT_HELLO_METHOD,
  AGENT_HELLO_METHOD,
  RESPONSE_TYPE_EVENT_FIRE,
  RpcFrameAssembler,
  SendOnceLoop,
  SessionTargetMapper,
  SingleConnectionGate,
  V4_WIRE_PROTOCOL_VERSION,
  decodeChannelMessage,
  encodeChannelRequest,
  encodeRpcFrame,
  type BoundTarget,
  type DecodedChannelMessage,
  type RpcFrameIdentity,
  type TaskListingRow,
  type WorkspaceListingEntry,
} from "./sessionBridge.ts";
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
  /** 无自动重连：任何 error/close 都是终态。 */
  reconnectPolicy: "never" as const,
  /** 收到 KICKED 立即退出并保持退出（不与用户官方页面互踢）。 */
  stopOnKicked: true,
  /**
   * mapping 阶段只读白名单（bootstrap-request / workspace-list-request，B1 已验证）；
   * send 阶段唯一追加的写向 = workspace-bridge-open（bridge 打开是发送链的官方前置，
   * 决策文件 §7），且必须 --send-authorized 显式解锁。
   */
  mappingReadonly: true,
  /** 发送步硬闸：缺 --send-authorized 时映射完成后即停，绝不构造发送帧。 */
  sendRequiresExplicitFlag: true,
  /** 一次运行的发送帧数上限（SendOnceLoop 结构性恒 1）。 */
  maxSendFrames: 1,
  /**
   * connect→teardown 的声明性预算（ms；不是 wall-clock 强杀）：配对窗口 30s +
   * 2×10s 只读查询 + 8s 收帧窗 + 10s bridge-open + 10s 握手/订阅 + 20s ACK 窗 +
   * 30s 活动窗 ≈ 148s。OAuth 回调等待单独有界（--wait-ms）。时限由各阶段自有界承担。
   */
  maxDurationMs: 150_000,
  /** 凭据只经运行期输入（浏览器授权 / stdin），禁止写盘、禁止进环境变量与日志。 */
  credentialsInMemoryOnly: true,
} as const;

/** 唯一放行开关：真实连接必须由操作员显式提供，仓库内不得出现其真值。 */
const UNLOCK_FLAG = "--i-know-this-connects";
/** 发送步放行开关：缺省映射后即停并打印 WOULD-send；绝不静默发送。 */
const SEND_FLAG = "--send-authorized";

const RELAY_HTTP_TIMEOUT_MS = 10_000;
/** 单个只读查询超时：官方 payloadRequester 默认 10s（payloadRequester.ts:67）。 */
const READ_QUERY_TIMEOUT_MS = 10_000;
/** 映射收帧窗：paired 后被动收 rpc-frame 的有界窗口。 */
const CAPTURE_WINDOW_MS = 8_000;
/** workspace-bridge-open 等待 ready 的有界窗口。 */
const BRIDGE_OPEN_TIMEOUT_MS = 10_000;
/** bridge RPC（hello/clientHello/subscribe）单请求有界窗口。 */
const BRIDGE_RPC_TIMEOUT_MS = 10_000;
/** 发送 ACK 窗口（CommandAck 应答；超窗 = unknown，绝不重发）。 */
const SEND_ACK_TIMEOUT_MS = 20_000;
/** 受理后的轮次活动观察窗。 */
const SEND_ACTIVITY_TIMEOUT_MS = 30_000;
/** JWT 形状闸：三段 base64url（只判形，不解码不校验签名）。 */
const JWT_SHAPE = /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/;
/** 默认产品 DB 路径（只读打开）。 */
const DEFAULT_DB_PATH = "~/.tuvis/tuvis.db";
/** 本连接的映射代际（本探针单连接、无重连，代际恒 1；换代只发生在新的进程运行）。 */
const CONNECTION_GENERATION = 1;

/** mapping 阶段允许的 app payload 类型（B1 白名单）；send 阶段追加 bridge-open。 */
const SEND_PHASE_ALLOWED_TYPES = [...READONLY_REQUEST_TYPES, "workspace-bridge-open"] as const;
type AppPayloadRequestType = (typeof SEND_PHASE_ALLOWED_TYPES)[number];

export type SessionProbeStep = {
  id: string;
  title: string;
  requires: string;
  detail: string;
};

/** 探针步骤说明（--help 打印；与实现一一对应）。 */
export const SESSION_PROBE_STEPS: readonly SessionProbeStep[] = [
  {
    id: "s1-acquire-link",
    title: "取当前有效官方链接",
    requires: "用户本人经 A4 设置卡保存（或 --sid 现场指定）",
    detail:
      "只读打开 ~/.tuvis/tuvis.db 的 settings 读 remote.zcode_official_url，按 A4 合同复验并解析 sid/mid。",
  },
  {
    id: "s2-account-jwt",
    title: "probe-oauth：账号 JWT（B1 已定案 proven；默认 bigmodel 通道）",
    requires: "用户本人在浏览器完成官方 OAuth",
    detail: "回环 redirect → code 换 JWT；JWT 只存内存。--jwt-stdin 可跳过（重跑模式）。",
  },
  {
    id: "s3-s4-connect",
    title: "一次性 token → 单条 WS 连接（SingleConnectionGate 唯一放行）",
    requires: "s2 的 JWT + 官方链接 sid/mid；用户确认此刻不依赖官方网页会话（同 role 互踢）",
    detail:
      "wss://<主机>/api/v1/remote/ws2?token=<一次性>&mid=<mid>；auth_init→auth_ack→paired；" +
      "waiting = 终端尚未可路由到设备（30s 路由窗口）；KICKED/失败即终态停止。",
  },
  {
    id: "s5-mapping",
    title: "只读映射：bootstrap / workspace-list + 被动收帧（精确 id）",
    requires: "s4 进入 paired",
    detail:
      "白名单两个只读查询 + 有界收帧窗组装 tasks-index 推送；产出 workspace 条目与 task 行" +
      "（精确 taskId / workspacePath），全程脱敏落证。",
  },
  {
    id: "s6-designate",
    title: "目标指称 → 精确 id（唯一匹配，绝不按标题发送）",
    requires: "--workspace（默认 Travel）/ --task-title（默认 hi）与 listing 精确唯一对应",
    detail:
      "指称只用于人读定位；发送授权用的是解析出的精确 id；不唯一/不命中 → 中止（映射证据照常落盘）。",
  },
  {
    id: "s7-send",
    title: "单次发送（--send-authorized 显式解锁；缺省只打印 WOULD-send）",
    requires: "batch-1 授权（会话 hi 发送一条消息）+ --send-authorized",
    detail:
      "workspace-bridge-open → ready → hello/clientHello → subscribe sessionId=<精确 taskId> → " +
      "sendConversationCommandV4（sendText，正文 = [tuvis-b2-marker:<UTC>]，模型/模式缺省）→ " +
      "记录 accepted/completed/unknown 全生命周期；恰一个 rpc-frame；未知不重发。",
  },
  {
    id: "s8-teardown",
    title: "断开与证据落盘",
    requires: "无",
    detail: "主动 close；脱敏 transcript.json + run.md 写入私有运行目录（0600）。",
  },
];

type ProbeFlags = {
  unlock: boolean;
  sendAuthorized: boolean;
  open: boolean;
  oauthWaitMs: number;
  jwtStdin: boolean;
  sid: string | null;
  dbPath: string | null;
  provider: OAuthProviderChannel;
  workspace: string;
  taskTitle: string;
  help: boolean;
};

const DEFAULT_WORKSPACE_DESIGNATION = "Travel";
const DEFAULT_TASK_DESIGNATION = "hi";

function parseFlags(argv: readonly string[]): ProbeFlags | string {
  const flags: ProbeFlags = {
    unlock: false,
    sendAuthorized: false,
    open: false,
    oauthWaitMs: 300_000,
    jwtStdin: false,
    sid: null,
    dbPath: null,
    provider: "bigmodel",
    workspace: DEFAULT_WORKSPACE_DESIGNATION,
    taskTitle: DEFAULT_TASK_DESIGNATION,
    help: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--i-know-this-connects") flags.unlock = true;
    else if (arg === "--send-authorized") flags.sendAuthorized = true;
    else if (arg === "--open") flags.open = true;
    else if (arg === "--jwt-stdin") flags.jwtStdin = true;
    else if (arg === "--help" || arg === "-h") flags.help = true;
    else if (arg.startsWith("--workspace=")) {
      const value = arg.slice("--workspace=".length).trim();
      if (!value || value.length > 128) return "--workspace 必须是非空且 ≤128 字符";
      flags.workspace = value;
    } else if (arg.startsWith("--task-title=")) {
      const value = arg.slice("--task-title=".length).trim();
      if (!value || value.length > 256) return "--task-title 必须是非空且 ≤256 字符";
      flags.taskTitle = value;
    } else if (arg === "--wait-ms" || arg.startsWith("--wait-ms=")) {
      const raw = arg === "--wait-ms" ? argv[i + 1] : arg.slice("--wait-ms=".length);
      if (raw === undefined) return "--wait-ms 需要一个毫秒值";
      const value = Number(raw);
      if (!Number.isFinite(value) || value <= 0 || value > 3_600_000) {
        return `--wait-ms 必须是 (0, 3600000] 内的毫秒数，收到：${raw}`;
      }
      flags.oauthWaitMs = value;
      if (arg === "--wait-ms") i += 1;
    } else if (arg.startsWith("--provider=")) {
      const value = arg.slice("--provider=".length).trim();
      if (value === "zai" || value === "bigmodel") flags.provider = value;
      else return `--provider 只接受 zai | bigmodel，收到：${value || "(空)"}`;
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
  lines.push("zcode-bridge-probe / session-probe — B2 会话映射与单次发送探针");
  lines.push("");
  lines.push("用法：node scripts/zcode-bridge-probe/session-probe.ts <flags>");
  lines.push("");
  lines.push("安全闸（硬编码，不可放宽）：");
  lines.push(`  ${JSON.stringify(PROBE_GUARDS)}`);
  lines.push("");
  lines.push("参数：");
  lines.push(`  ${UNLOCK_FLAG}                必需。确认「我知道这会发起真实连接」。`);
  lines.push(
    `  ${SEND_FLAG}             发送步解锁（batch-1 授权：会话 ${DEFAULT_TASK_DESIGNATION} 发一条消息）。`
  );
  lines.push("                             缺席时映射完成后只打印 WOULD-send 即停，绝不发送。");
  lines.push(
    "  --workspace=<指称>        测试工作区指称（默认 Travel；listing label 精确唯一匹配）。"
  );
  lines.push("  --task-title=<指称>       测试会话指称（默认 hi；任务 title 精确唯一匹配，");
  lines.push("                             只用于定位，发送归因用解析出的精确 id）。");
  lines.push("  --provider=<zai|bigmodel> OAuth 通道（默认 bigmodel——B1 定案用户账号在该通道）。");
  lines.push("  --open                    授权阶段用系统默认浏览器打开授权 URL（默认只打印）。");
  lines.push("  --wait-ms=<ms>            OAuth 回调等待窗口（默认 300000，上限 3600000）。");
  lines.push("  --jwt-stdin               重跑模式：跳过 OAuth，从 stdin 读账号 JWT（只入内存）。");
  lines.push(
    "  --sid=<device_sid>        跳过 DB 读取，直接指定 device_sid（origin 取官方默认）。"
  );
  lines.push(`  --db=<path>               settings 库路径（默认 ${DEFAULT_DB_PATH}，只读打开）。`);
  lines.push("  --help                    打印本帮助。");
  lines.push("");
  lines.push("步骤：");
  for (const step of SESSION_PROBE_STEPS) {
    lines.push(`  [${step.id}] ${step.title}`);
    lines.push(`      前置：${step.requires}`);
    lines.push(`      ${step.detail}`);
  }
  lines.push("");
  lines.push("用户交互：");
  lines.push("  · 授权由你本人在浏览器完成（探针不碰密码、不代登）；");
  lines.push(
    "  · 探针连接可能踢掉你正在用的官方网页会话（同 role 单 owner），请在此时段不依赖网页端；"
  );
  lines.push("  · 发送步只会向指定会话注入一条合成 marker 消息（内容仅含 UTC 时间戳），");
  lines.push("    不审批、不打断、不改模型；结果未知时绝不重发。");
  lines.push("");
  lines.push(
    `证据：~/Documents/tuvis-verify-logs/zcode/macos-B2<UTC 时间戳>-<随机码>/（0600，脱敏）。`
  );
  console.log(lines.join("\n"));
}

function log(line: string): void {
  console.log(`[b2-probe] ${line}`);
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

function relayWsUrlFromOrigin(origin: string): string {
  const parsed = new URL(origin);
  const protocol = parsed.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${parsed.host}${RELAY_WS_PATH}`;
}

// ---------------------------------------------------------------------------
// B2 payload 通道：mapping 阶段白名单只读请求 + rpc-frame 被动组装 + bridge 请求
// ---------------------------------------------------------------------------

type AppPayloadOutcome =
  { kind: "ok"; payload: Record<string, unknown> } | { kind: "send-failed" } | { kind: "timeout" };

type BridgeRpcOutcome =
  { kind: "ok"; decoded: DecodedChannelMessage } | { kind: "send-failed" } | { kind: "timeout" };

class B2Channel {
  private readonly allowedTypes: readonly AppPayloadRequestType[];
  private readonly pendingApp = new Map<string, (payload: Record<string, unknown>) => void>();
  private readonly pendingBridge = new Map<number, (decoded: DecodedChannelMessage) => void>();
  private attachedConn?: { onData?: (payload: unknown) => void };
  /** 被动组装器（mapping 阶段绑定到宿主推送的 bridge 身份）。 */
  private readonly capture = new RpcFrameAssembler();
  /** 未经 request 匹配的入站 data payload（脱敏前原样持有在内存）。 */
  readonly unsolicited: unknown[] = [];
  /** 被动组装完成的 Channel 消息文本（收帧窗内；含 tasks-index 等推送）。 */
  readonly bridgeMessages: string[] = [];
  /** 收到的 rpc-frame 原始 payload（含 dataBase64；只落私有证据）。 */
  readonly rpcFramesSeen: unknown[] = [];
  /** 收到的 rpc-frame-ack 数（去重前）。 */
  rpcAckCount = 0;
  /** 组装器判定的重复/故障计数。 */
  rpcDuplicateCount = 0;
  rpcFaultCount = 0;
  /** send 阶段钩子：每条入站 rpc-frame 转交 SendOnceLoop（onData 单槽的唯一出口）。 */
  onRpcFrame?: (payload: unknown) => void;

  constructor(options: { allowedTypes: readonly AppPayloadRequestType[] }) {
    this.allowedTypes = options.allowedTypes;
  }

  attach(conn: { onData?: (payload: unknown) => void }): void {
    this.attachedConn = conn;
    conn.onData = (payload) => this.handle(payload);
  }

  detach(): void {
    if (this.attachedConn) this.attachedConn.onData = undefined;
    this.attachedConn = undefined;
  }

  /** mapping 白名单只读请求（bootstrap-request / workspace-list-request）。 */
  request(
    sender: { sendData(payload: unknown): boolean },
    type: (typeof READONLY_REQUEST_TYPES)[number],
    timeoutMs: number
  ): Promise<AppPayloadOutcome> {
    if (!READONLY_REQUEST_TYPES.includes(type)) {
      return Promise.resolve({ kind: "send-failed" });
    }
    return this.requestPayload(sender, type, {}, timeoutMs);
  }

  /** send 阶段唯一追加的写向：workspace-bridge-open（仍过白名单闸）。 */
  openWorkspaceBridge(
    sender: { sendData(payload: unknown): boolean },
    payload: { requestId: string; bridgeSessionId: string; workspaceKey: string; taskId: string },
    timeoutMs: number
  ): Promise<AppPayloadOutcome> {
    return this.requestPayload(sender, "workspace-bridge-open", payload, timeoutMs);
  }

  private requestPayload(
    sender: { sendData(payload: unknown): boolean },
    type: AppPayloadRequestType,
    extra: Record<string, unknown>,
    timeoutMs: number
  ): Promise<AppPayloadOutcome> {
    // 运行期白名单闸（PROBE_GUARDS.mappingReadonly / send 阶段显式追加的唯一写向）。
    if (!this.allowedTypes.includes(type)) {
      throw new Error(`payload-whitelist: type not allowed: ${type}`);
    }
    const requestId =
      typeof extra.requestId === "string"
        ? extra.requestId
        : `b2-${type}-${crypto.randomBytes(6).toString("hex")}`;
    return new Promise<AppPayloadOutcome>((resolve) => {
      const timer = setTimeout(
        () => {
          this.pendingApp.delete(requestId);
          resolve({ kind: "timeout" });
        },
        Math.max(1, timeoutMs)
      );
      this.pendingApp.set(requestId, (payload) => {
        clearTimeout(timer);
        resolve({ kind: "ok", payload });
      });
      const sent = sender.sendData({ zcode_type: type, requestId, ...extra });
      if (!sent) {
        clearTimeout(timer);
        this.pendingApp.delete(requestId);
        resolve({ kind: "send-failed" });
      }
    });
  }

  /** bridge rpc-frame 通道请求（hello/clientHello/subscribe；发送帧由 SendOnceLoop 自理）。 */
  bridgeRequest(
    sender: { sendData(payload: unknown): boolean },
    args: {
      id: number;
      method: string;
      params?: unknown;
      identity: RpcFrameIdentity;
      seq: number;
      messageSeq: number;
    },
    timeoutMs: number
  ): Promise<BridgeRpcOutcome> {
    const bytes = encodeChannelRequest({
      id: args.id,
      channel: AGENT_CHANNEL,
      method: args.method,
      params: args.params,
    });
    const frame = encodeRpcFrame({
      identity: args.identity,
      seq: args.seq,
      messageSeq: args.messageSeq,
      bytes,
    });
    return new Promise<BridgeRpcOutcome>((resolve) => {
      const timer = setTimeout(
        () => {
          this.pendingBridge.delete(args.id);
          resolve({ kind: "timeout" });
        },
        Math.max(1, timeoutMs)
      );
      this.pendingBridge.set(args.id, (decoded) => {
        clearTimeout(timer);
        resolve({ kind: "ok", decoded });
      });
      const sent = sender.sendData(frame);
      if (!sent) {
        clearTimeout(timer);
        this.pendingBridge.delete(args.id);
        resolve({ kind: "send-failed" });
      }
    });
  }

  private handle(payload: unknown): void {
    if (payload && typeof payload === "object") {
      const record = payload as { zcode_type?: unknown; requestId?: unknown };
      if (record.zcode_type === "rpc-frame") {
        this.rpcFramesSeen.push(payload);
        this.onRpcFrame?.(payload);
        const event = this.capture.accept(payload);
        if (event.kind === "duplicate") {
          this.rpcDuplicateCount += 1;
          return;
        }
        if (event.kind === "fault") {
          this.rpcFaultCount += 1;
          return;
        }
        if (event.kind === "incomplete") return;
        const decoded = decodeChannelMessage(event.bytes);
        const rawJson = Buffer.from(event.bytes).toString("utf8");
        if (this.bridgeMessages.length < 64) this.bridgeMessages.push(rawJson);
        if (decoded && decoded.id !== null && this.pendingBridge.has(decoded.id)) {
          const resolve = this.pendingBridge.get(decoded.id);
          this.pendingBridge.delete(decoded.id);
          resolve?.(decoded);
        }
        return;
      }
      if (record.zcode_type === "rpc-frame-ack") {
        // 先转发给 send 阶段钩子（SendOnceLoop 据此记 peerAcknowledged 并做 ack 去重），
        // 再计数；mapping 阶段 onRpcFrame 未挂 → 转发为 no-op。
        this.onRpcFrame?.(payload);
        this.rpcAckCount += 1;
        return;
      }
      if (typeof record.requestId === "string" && this.pendingApp.has(record.requestId)) {
        const resolve = this.pendingApp.get(record.requestId);
        this.pendingApp.delete(record.requestId);
        resolve?.(payload as Record<string, unknown>);
        return;
      }
    }
    this.unsolicited.push(payload);
  }
}

// ---------------------------------------------------------------------------
// listing 采集：workspace 条目与 task 行（精确 id）从真实帧里结构化收割
// ---------------------------------------------------------------------------

function harvestTaskRows(value: unknown, out: TaskListingRow[]): void {
  if (Array.isArray(value)) {
    for (const item of value) harvestTaskRows(item, out);
    return;
  }
  if (!value || typeof value !== "object") return;
  const record = value as Record<string, unknown>;
  const address = record.address;
  if (address && typeof address === "object") {
    const addr = address as {
      taskId?: unknown;
      workspacePath?: unknown;
      workspaceIdentity?: unknown;
    };
    if (typeof addr.taskId === "string" && typeof addr.workspacePath === "string") {
      const meta = record.meta;
      const title =
        meta && typeof meta === "object" && typeof (meta as { title?: unknown }).title === "string"
          ? ((meta as { title: string }).title as string)
          : undefined;
      out.push({
        address: {
          taskId: addr.taskId,
          workspacePath: addr.workspacePath,
          ...(typeof addr.workspaceIdentity === "string"
            ? { workspaceIdentity: addr.workspaceIdentity }
            : {}),
        },
        ...(title !== undefined ? { meta: { title } } : {}),
      });
    }
  }
  for (const item of Object.values(record)) harvestTaskRows(item, out);
}

function harvestWorkspaceEntries(
  payloads: Array<Record<string, unknown> | null>,
  out: WorkspaceListingEntry[]
): void {
  for (const payload of payloads) {
    if (!payload) continue;
    const result = payload.result;
    if (!result || typeof result !== "object") continue;
    const workspaces = (result as { workspaces?: unknown }).workspaces;
    if (!Array.isArray(workspaces)) continue;
    for (const entry of workspaces) {
      if (!entry || typeof entry !== "object") continue;
      const record = entry as Record<string, unknown>;
      if (typeof record.workspacePath !== "string") continue;
      out.push({
        workspacePath: record.workspacePath,
        ...(typeof record.workspaceIdentity === "string"
          ? { workspaceIdentity: record.workspaceIdentity }
          : {}),
        ...(typeof record.remoteSessionId === "string"
          ? { remoteSessionId: record.remoteSessionId }
          : {}),
        ...(typeof record.label === "string" ? { label: record.label } : {}),
        ...(typeof record.kind === "string" ? { kind: record.kind } : {}),
      });
    }
  }
}

function dedupeRows(rows: TaskListingRow[]): TaskListingRow[] {
  const seen = new Set<string>();
  const out: TaskListingRow[] = [];
  for (const row of rows) {
    const key = `${row.address.workspacePath}\u0000${row.address.taskId}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(row);
  }
  return out;
}

// ---------------------------------------------------------------------------
// 探针主体
// ---------------------------------------------------------------------------

type ProbeOutcome = {
  oauthVerdict: string;
  connectVerdict: string;
  mappingVerdict: string;
  sendVerdict: string;
  exitCode: number;
};

type RunContext = {
  redactor: SecretRedactor;
  run: { dir: string; runId: string };
  startedIso: string;
  flags: ProbeFlags;
  events: Array<Record<string, unknown>>;
};

/** 探针在飞阶段标签（故障围栏用：fault 时给对应阶段记 STOPPED:fault-<阶段>）。 */
type ProbeStage = "oauth" | "connect" | "mapping" | "send" | "teardown";

function record(ctx: RunContext, step: string, message: string, data?: unknown): void {
  ctx.events.push({
    t: new Date().toISOString(),
    step,
    message,
    ...(data !== undefined ? { data } : {}),
  });
}

export async function runProbe(argv: readonly string[]): Promise<ProbeOutcome> {
  const parsed = parseFlags(argv);
  if (typeof parsed === "string") {
    console.error(`[b2-probe] 参数错误：${parsed}`);
    printHelp();
    return {
      oauthVerdict: "NOT-RUN",
      connectVerdict: "NOT-RUN",
      mappingVerdict: "NOT-RUN",
      sendVerdict: "NOT-RUN",
      exitCode: 2,
    };
  }
  if (parsed.help) {
    printHelp();
    return {
      oauthVerdict: "NOT-RUN",
      connectVerdict: "NOT-RUN",
      mappingVerdict: "NOT-RUN",
      sendVerdict: "NOT-RUN",
      exitCode: 0,
    };
  }
  if (!parsed.unlock) {
    printHelp();
    console.error(
      `\n[b2-probe] 拒绝执行：缺少 ${UNLOCK_FLAG}。本探针会发起真实网络连接 ` +
        `(bigmodel.cn / zcode.z.ai / 本地回环回调)，必须由操作员显式解锁。`
    );
    return {
      oauthVerdict: "NOT-RUN",
      connectVerdict: "NOT-RUN",
      mappingVerdict: "NOT-RUN",
      sendVerdict: "NOT-RUN",
      exitCode: 2,
    };
  }

  const redactor = new SecretRedactor();
  const run = createEvidenceRunDir(undefined, "B2");
  const startedIso = new Date().toISOString();
  const ctx: RunContext = { redactor, run, startedIso, flags: parsed, events: [] };
  log(`证据目录（私有，勿提交/外发）：${run.dir}`);

  // —— 故障围栏（评审 Important 修复）：主体全程 try/catch——任何阶段抛出（含
  // SendOnceLoop 构造）都不得变成无证据的裸 crash；catch 里记账、尽力清理连接、
  // 给在飞阶段记结构化 STOPPED:fault-<阶段> verdict 并照常落证。
  let jwt: string | null = null;
  let oauthVerdict = "SKIPPED";
  let connectVerdict = "SKIPPED";
  let mappingVerdict = "SKIPPED";
  let sendVerdict = "SKIPPED";
  const oauthSummary: Record<string, unknown> = {
    attempted: !parsed.jwtStdin,
    provider: parsed.provider,
  };
  const connectSummary: Record<string, unknown> = { attempted: false };
  const mappingSummary: Record<string, unknown> = { attempted: false };
  const sendSummary: Record<string, unknown> = { attempted: false };
  let stage: ProbeStage = "oauth";
  // 存活连接登记（fault 路径的尽力清理用；正常路径各自的显式 teardown 不受影响）。
  let liveGate: SingleConnectionGate | null = null;
  let liveConn: RelayTerminalConnection | null = null;
  const liveChannels: B2Channel[] = [];
  const cleanupLive = (): void => {
    for (const channel of liveChannels.splice(0)) {
      try {
        channel.detach();
      } catch {
        // 尽力清理：fault 路径不允许二次抛出。
      }
    }
    try {
      liveConn?.close();
    } catch {
      // 尽力清理。
    }
    liveConn = null;
    try {
      liveGate?.release();
    } catch {
      // 尽力清理。
    }
    liveGate = null;
  };

  try {
    // —— s2：probe-oauth（默认 bigmodel——B1 定案用户账号在该通道；verdict/summary
    // 已在围栏区声明，fault 路径也要能读到） ————————————
    if (parsed.jwtStdin) {
      record(ctx, "probe-oauth", "skipped", { reason: "jwt-stdin rerun" });
      log("probe-oauth 跳过（--jwt-stdin 重跑模式）。");
    } else {
      const isZai = parsed.provider === "zai";
      record(ctx, "probe-oauth", "start", {
        provider: parsed.provider,
        authorizeOrigin: isZai ? OFFICIAL_AUTHORIZE_ORIGIN : OFFICIAL_BIGMODEL_AUTHORIZE_ORIGIN,
        authorizePath: isZai ? OFFICIAL_AUTHORIZE_PATH : OFFICIAL_BIGMODEL_AUTHORIZE_PATH,
        clientIdentifierField: isZai ? "client_id" : "appId",
        clientIdentifier: isZai ? OFFICIAL_OAUTH_CLIENT_ID : OFFICIAL_BIGMODEL_APP_ID,
      });
      const state = generateOAuthState();
      redactor.register("state", state);
      const server = await LoopbackCallbackServer.start();
      let callback: LoopbackCallback;
      const redirectUri = server.redirectUri;
      try {
        const authorizeUrl = buildAuthorizeUrl(parsed.provider, { redirectUri, state });
        redactor.register("link", authorizeUrl);
        oauthSummary.redirect = { host: "127.0.0.1", port: server.port, path: "/callback" };
        record(ctx, "probe-oauth", "listening", { redirectPort: server.port });
        log(
          parsed.provider === "bigmodel"
            ? "请在浏览器打开下面的官方授权 URL，并用你的国内智谱（bigmodel.cn）账号完成授权："
            : "请在浏览器打开下面的官方授权 URL，并用你的 ZCode 账号完成授权："
        );
        log(`  ${authorizeUrl}`);
        if (parsed.open) {
          log(
            openInBrowser(authorizeUrl)
              ? "已请求系统浏览器打开（--open）。"
              : "--open 失败，请手动复制 URL。"
          );
        }
        log(`等待回环回调（有界 ${parsed.oauthWaitMs} ms）……`);
        callback = await server.waitCallback(state, parsed.oauthWaitMs);
      } finally {
        await server.close();
      }
      if (callback.kind === "code") {
        redactor.register("auth-code", callback.code);
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
          };
          record(ctx, "probe-oauth", "exchange-ok", {
            verdict: "PROVEN",
            masked: maskJwt(jwt),
            expiresInSeconds: exchange.expiresInSeconds,
          });
          log("probe-oauth verdict = PROVEN。");
        } else {
          oauthVerdict = exchange.kind === "rejected" ? "UNSUPPORTED" : "INCONCLUSIVE";
          oauthSummary.verdict = oauthVerdict;
          oauthSummary.stage = "token-exchange";
          oauthSummary.reason = exchange.kind === "rejected" ? exchange.reason : "unreachable";
          record(ctx, "probe-oauth", "exchange-failed", { verdict: oauthVerdict });
          log(`probe-oauth verdict = ${oauthVerdict}。`);
        }
      } else {
        oauthVerdict = "UNSUPPORTED";
        oauthSummary.verdict = "UNSUPPORTED";
        oauthSummary.stage = callback.kind === "authorize-error" ? "authorize" : "callback-wait";
        record(ctx, "probe-oauth", "callback-failed", { kind: callback.kind });
        log(`probe-oauth verdict = UNSUPPORTED（${callback.kind}）。`);
      }
    }

    // —— s1/s3/s4：链接 → token → 单连接 ————————————————————————————————————
    stage = "connect";
    if (parsed.jwtStdin) {
      const raw = (await readAllStdin()).trim();
      if (!JWT_SHAPE.test(raw)) {
        record(ctx, "connect", "jwt-invalid-shape", {});
        console.error("[b2-probe] stdin 内容不是 JWT 形状，拒绝执行。");
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return {
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict: "ABORTED:jwt-shape",
          exitCode: 2,
        };
      }
      jwt = raw;
      redactor.register("jwt", jwt);
      connectSummary.jwt = { alias: "JWT_1", masked: maskJwt(jwt) };
    }

    const gate = new SingleConnectionGate();
    liveGate = gate;
    if (jwt) {
      connectSummary.attempted = true;
      let sid: string;
      let mid: string | null = null;
      let origin = OFFICIAL_ZCODE_ENDPOINT_ORIGIN;
      if (parsed.sid) {
        sid = parsed.sid;
        redactor.register("sid", sid);
        connectSummary.linkSource = "cli";
      } else {
        const dbPath = parsed.dbPath ?? path.join(os.homedir(), ".tuvis", "tuvis.db");
        const settings = readOfficialLinkFromSettings(dbPath);
        if (settings.kind !== "ok") {
          connectVerdict = `ABORTED:settings-${settings.kind}`;
          record(ctx, "connect", "settings-read-failed", { kind: settings.kind });
          log(`connect 中止：无法从 ${dbPath} 读取官方链接（${settings.kind}）。`);
          await writeEvidence(
            ctx,
            oauthVerdict,
            connectVerdict,
            mappingVerdict,
            sendVerdict,
            oauthSummary,
            connectSummary,
            mappingSummary,
            sendSummary
          );
          return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 2 };
        }
        const parsedLink = parseOfficialLink(settings.link);
        if (!parsedLink.ok) {
          connectVerdict = `ABORTED:link-invalid-${parsedLink.reason}`;
          record(ctx, "connect", "link-invalid", { reason: parsedLink.reason });
          log(`connect 中止：已保存链接未通过 A4 复验（${parsedLink.reason}）。`);
          await writeEvidence(
            ctx,
            oauthVerdict,
            connectVerdict,
            mappingVerdict,
            sendVerdict,
            oauthSummary,
            connectSummary,
            mappingSummary,
            sendSummary
          );
          return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 2 };
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
          origin: parsedLink.origin,
          pathVersion: parsedLink.pathVersion,
          sid: "SID_A",
          mid: parsedLink.mid ? "MID_A" : null,
        };
      }

      const relayWsUrl = relayWsUrlFromOrigin(origin);
      connectSummary.relay = { origin, wsUrl: relayWsUrl, midAttached: mid !== null };
      const token = await requestTerminalToken({
        relayOrigin: origin,
        accountJwt: jwt,
        signal: AbortSignal.timeout(RELAY_HTTP_TIMEOUT_MS),
      });
      if (token.kind !== "ok") {
        connectVerdict = `STOPPED:token-${token.kind}`;
        record(ctx, "connect", "token-request-failed", { kind: token.kind });
        log(`connect 停止：token 申请失败（${token.kind}），不重试。`);
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }
      redactor.register("relay-token", token.token);
      connectSummary.terminalToken = {
        alias: "TOK_1",
        masked: maskOpaque(token.token),
        expiresInSeconds: token.expiresInSeconds,
      };
      record(ctx, "connect", "terminal-token", {
        masked: maskOpaque(token.token),
        expiresInSeconds: token.expiresInSeconds,
      });
      log(`terminal token OK（expires_in=${token.expiresInSeconds}s，一次性）。`);

      log("建立单条 WS 连接（SingleConnectionGate 唯一放行；30s 路由窗口）……");
      const acquired = gate.acquire<RelayTerminalConnection>(
        () =>
          new RelayTerminalConnection({
            relayWsUrl,
            token: token.token,
            deviceSid: sid,
            deviceMid: mid,
          })
      );
      if (!acquired.ok) {
        // 结构性不可达：gate 在 active 之外不会拒绝第一次 acquire。
        connectVerdict = "STOPPED:gate";
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 1 };
      }
      const conn = acquired.connection;
      liveConn = conn;
      const channel = new B2Channel({ allowedTypes: READONLY_REQUEST_TYPES });
      liveChannels.push(channel);
      channel.attach(conn);
      const outcome = await conn.connect();
      if (outcome !== "paired") {
        connectVerdict = `STOPPED:${outcome}`;
        record(ctx, "connect", "handshake-terminal", { outcome });
        log(`connect 停止：握手终态 = ${outcome}（无重试）。`);
        conn.close();
        channel.detach();
        gate.release();
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }
      connectVerdict = "PAIRED";
      record(ctx, "connect", "paired", {});
      log("已配对。开始只读映射（bootstrap → workspace-list + 收帧窗）……");

      // —— s5：mapping ————————————————————————————————————————————————————————
      stage = "mapping";
      mappingSummary.attempted = true;
      const bootstrap = await channel.request(conn, "bootstrap-request", READ_QUERY_TIMEOUT_MS);
      const workspaceList = await channel.request(
        conn,
        "workspace-list-request",
        READ_QUERY_TIMEOUT_MS
      );
      record(ctx, "mapping", "bootstrap-request", summarizePayload(bootstrap));
      record(ctx, "mapping", "workspace-list-request", summarizePayload(workspaceList));
      log(
        `bootstrap-request → ${bootstrap.kind}；workspace-list-request → ${workspaceList.kind}。`
      );

      log(`被动收帧窗 ${CAPTURE_WINDOW_MS} ms（tasks-index 等宿主推送，B1 实连同款）……`);
      await new Promise((resolve) => setTimeout(resolve, CAPTURE_WINDOW_MS));

      // 结构化收割（真实值只在内存；登记别名后只落别名证据）。
      const workspaces: WorkspaceListingEntry[] = [];
      harvestWorkspaceEntries(
        [
          bootstrap.kind === "ok" ? bootstrap.payload : null,
          workspaceList.kind === "ok" ? workspaceList.payload : null,
        ],
        workspaces
      );
      const rawRows: TaskListingRow[] = [];
      for (const message of channel.bridgeMessages) {
        try {
          harvestTaskRows(JSON.parse(message) as unknown, rawRows);
        } catch {
          // 非 JSON 消息跳过（计数即可）。
        }
      }
      const tasks = dedupeRows(rawRows);
      for (const entry of workspaces) {
        redactor.register("path", entry.workspacePath);
        if (entry.workspaceIdentity) redactor.register("sid", entry.workspaceIdentity);
        if (entry.remoteSessionId) redactor.register("sid", entry.remoteSessionId);
        if (entry.label) redactor.register("name", entry.label);
      }
      for (const row of tasks) {
        redactor.register("path", row.address.workspacePath);
        redactor.register("sid", row.address.taskId);
        if (row.meta?.title) redactor.register("name", row.meta.title);
      }
      mappingSummary.listings = {
        bootstrapOutcome: bootstrap.kind,
        workspaceListOutcome: workspaceList.kind,
        workspaceEntries: workspaces.length,
        taskRows: tasks.length,
        bridgeMessages: channel.bridgeMessages.length,
        rpcFramesSeen: channel.rpcFramesSeen.length,
        rpcAckCount: channel.rpcAckCount,
        rpcDuplicateCount: channel.rpcDuplicateCount,
        rpcFaultCount: channel.rpcFaultCount,
      };
      record(ctx, "mapping", "harvest", mappingSummary.listings);
      recordChannelEvidence(ctx, "mapping", channel);
      log(
        `映射输入：workspace 条目 ×${workspaces.length}，task 行 ×${tasks.length}（收帧 ${channel.bridgeMessages.length} 条）。`
      );

      // —— s6：designate（指称 → 精确 id；唯一匹配，绝不按标题发送） ————————————
      const designation = designateTarget(
        ctx,
        workspaces,
        tasks,
        parsed.workspace,
        parsed.taskTitle
      );
      if (!designation.ok) {
        mappingVerdict = `ABORTED:designate-${designation.reason}`;
        record(ctx, "mapping", "designate-failed", {
          reason: designation.reason,
          workspaceDesignation: redactor.sanitizeText(parsed.workspace),
          taskDesignation: redactor.sanitizeText(parsed.taskTitle),
        });
        log(`映射中止：${designation.reason}（未发送任何写向；未匹配的指称不入证据正文）。`);
        conn.close();
        channel.detach();
        gate.release();
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }
      const bound: BoundTarget = designation.bound;
      try {
        const mapper = SessionTargetMapper.fromListings({
          workspaces,
          tasks,
          generation: CONNECTION_GENERATION,
        });
        mapper.authorizeSend(bound, CONNECTION_GENERATION);
      } catch (error) {
        mappingVerdict = "ABORTED:generation";
        record(ctx, "mapping", "generation-check-failed", {
          reason: error instanceof Error ? error.message : String(error),
        });
        conn.close();
        channel.detach();
        gate.release();
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }
      mappingVerdict = "MAPPED";
      mappingSummary.target = {
        taskId: bound.taskId,
        workspacePath: bound.workspacePath,
        workspaceKey: bound.workspaceKey,
        generation: bound.generation,
      };
      record(ctx, "mapping", "designated", {
        taskId: bound.taskId,
        workspacePath: bound.workspacePath,
        workspaceKey: bound.workspaceKey,
        generation: bound.generation,
        note: "以上为 redactor 别名（transcript 落盘前统一 sanitizeDeep）",
      });
      log(
        `目标已锁定（别名）：taskId=${bound.taskId} workspace=${bound.workspaceKey}（generation ${bound.generation}）。`
      );

      // —— s7：send（缺 --send-authorized 只打印 WOULD-send） ————————————————
      stage = "send";
      if (!parsed.sendAuthorized) {
        sendVerdict = "MAPPED-NOT-SENT";
        sendSummary.attempted = false;
        sendSummary.reason = "missing --send-authorized";
        const marker = `[tuvis-b2-marker:${new Date().toISOString()}]`;
        sendSummary.wouldSend = {
          chain:
            "workspace-bridge-open{workspaceKey,taskId} → ready → rpc-frame 通道 " +
            "helloConversationV4 → initializeConversationV4(clientHello) → " +
            "subscribeConversationV4{workspacePath, workspaceIdentity?, sessionId} → " +
            "sendConversationCommandV4{workspacePath, workspaceIdentity?, envelope}",
          envelope: {
            commandId: "<uuid>",
            clientId: "<probe-client-id>",
            sessionId: bound.taskId,
            type: "sendText",
            payload: { text: marker },
            issuedAt: "<epoch-ms>",
          },
          note: "模型/思考强度/模式全缺省 = 沿用会话现值；不审批、不打断；恰一个 rpc-frame；未知不重发。",
        };
        record(ctx, "send", "would-send", sendSummary.wouldSend);
        log("未提供 --send-authorized：映射完成即停。将发送的内容已记录到证据（would-send）。");
        conn.close();
        channel.detach();
        gate.release();
        record(ctx, "teardown", "closed", {});
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }

      sendSummary.attempted = true;
      const sendChannel = new B2Channel({
        allowedTypes: SEND_PHASE_ALLOWED_TYPES,
      });
      liveChannels.push(sendChannel);
      sendChannel.attach(conn);
      const bridgeSessionId = `b2-bridge-${crypto.randomUUID()}`;
      redactor.register("sid", bridgeSessionId);
      // workspace-bridge-open 形状钉源 @ 3c5b2276：
      // WebRemoteControlWorkspaceBridgeOpenPayload{requestId, bridgeSessionId,
      // bridgeGeneration?, recoveryId?, workspaceKey, taskId?}（packages/shared/src/
      // web-remote-control.ts:435-447）；ready 回显同文件 :449-456；
      // workspaceKey = workspaceIdentity?.trim() || workspacePath（同文件 :743-747）。
      const bridgeOpen = await sendChannel.openWorkspaceBridge(
        conn,
        {
          requestId: `b2-bridge-open-${crypto.randomBytes(6).toString("hex")}`,
          bridgeSessionId,
          workspaceKey: bound.workspaceKey,
          taskId: bound.taskId,
        },
        BRIDGE_OPEN_TIMEOUT_MS
      );
      record(ctx, "send", "workspace-bridge-open", summarizePayload(bridgeOpen));
      if (bridgeOpen.kind !== "ok") {
        sendVerdict = `STOPPED:bridge-open-${bridgeOpen.kind}`;
        log(`send 停止：workspace-bridge-open 未得到 ready（${bridgeOpen.kind}）。`);
        conn.close();
        sendChannel.detach();
        channel.detach();
        gate.release();
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }
      const readyPayload = bridgeOpen.payload as {
        bridgeSessionId?: unknown;
        bridgeGeneration?: unknown;
        bridge?: { bridgeSessionId?: unknown; bridgeGeneration?: unknown; kind?: unknown };
      };
      const readyBridge = readyPayload.bridge;
      const resolvedBridgeSessionId =
        (typeof readyBridge?.bridgeSessionId === "string" && readyBridge.bridgeSessionId) ||
        (typeof readyPayload.bridgeSessionId === "string" && readyPayload.bridgeSessionId) ||
        bridgeSessionId;
      const bridgeGeneration =
        (typeof readyBridge?.bridgeGeneration === "number" && readyBridge.bridgeGeneration) ||
        (typeof readyPayload.bridgeGeneration === "number" && readyPayload.bridgeGeneration) ||
        CONNECTION_GENERATION;
      const identity: RpcFrameIdentity = {
        bridgeSessionId: resolvedBridgeSessionId,
        bridgeGeneration,
      };
      redactor.register("sid", resolvedBridgeSessionId);
      record(ctx, "send", "workspace-bridge-ready", {
        bridgeSessionId: resolvedBridgeSessionId,
        bridgeGeneration,
        kind: typeof readyBridge?.kind === "string" ? readyBridge.kind : null,
      });
      log(`workspace bridge 已就绪（bridgeGeneration=${bridgeGeneration}）。`);

      const clientId = `tuvis-b2-probe-${crypto.randomBytes(4).toString("hex")}`;
      let bridgeSeq = 0;
      let bridgeMessageSeq = 0;
      let bridgeId = 0;
      const bridgeCall = async (method: string, params: unknown): Promise<BridgeRpcOutcome> => {
        bridgeId += 1;
        bridgeSeq += 1;
        bridgeMessageSeq += 1;
        const result = await sendChannel.bridgeRequest(
          conn,
          { id: bridgeId, method, params, identity, seq: bridgeSeq, messageSeq: bridgeMessageSeq },
          BRIDGE_RPC_TIMEOUT_MS
        );
        record(ctx, "send", `bridge-${method}`, { id: bridgeId, outcome: result.kind });
        return result;
      };

      // 官方握手顺序 hello → clientHello（形状钉源 @ 3c5b2276：
      // packages/ui/src/v4/agentV4ConnectionHandshake.ts:30-46 —— hello 先行、
      // clientHello{kind, protocolVersion, clientId, clientKind, appVersion, capabilities?}，
      // clientKind = hello.clientMode === "desktop-continuous" ? "desktop" : "web"；
      // helloMessageSchema = {kind:"hello", protocolVersion, connectionId, clientMode:
      // "desktop-continuous"|"web-remote-replayable", deliveryProfile, serverTime,
      // capabilities, auth}（packages/shared/src/zcode-protocol-v4/transport.ts:42-65）；
      // clientHelloSchema 同文件 :69-92（capabilities 可选，缺省=旧客户端）；
      // capabilities.independentPlanState 见同文件 :23-39（hostCapabilitiesSchema）。
      // 探针省略 capabilities（optional，不声明任何扩展位=最小面）。
      const hello = await bridgeCall(AGENT_HELLO_METHOD, undefined);
      if (hello.kind !== "ok") {
        sendVerdict = `STOPPED:hello-${hello.kind}`;
        log(`send 停止：hello 未得到应答（${hello.kind}），不发送。`);
        await teardownSend(conn, sendChannel, channel, gate);
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }
      const helloBody = hello.decoded.body as {
        kind?: unknown;
        connectionId?: unknown;
        clientMode?: unknown;
        capabilities?: { independentPlanState?: unknown };
      } | null;
      if (!helloBody || helloBody.kind !== "hello" || typeof helloBody.connectionId !== "string") {
        sendVerdict = "STOPPED:hello-shape";
        record(ctx, "send", "hello-shape-unexpected", {});
        log("send 停止：hello 应答形状不符，不发送。");
        await teardownSend(conn, sendChannel, channel, gate);
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }
      const clientMode =
        helloBody.clientMode === "desktop-continuous"
          ? "desktop-continuous"
          : "web-remote-replayable";
      const clientHello = await bridgeCall(AGENT_CLIENT_HELLO_METHOD, {
        kind: "clientHello",
        protocolVersion: V4_WIRE_PROTOCOL_VERSION,
        clientId,
        clientKind: clientMode === "desktop-continuous" ? "desktop" : "web",
        appVersion: "tuvis-b2-probe",
      });
      if (clientHello.kind !== "ok") {
        sendVerdict = `STOPPED:clientHello-${clientHello.kind}`;
        log(`send 停止：clientHello 未得到应答（${clientHello.kind}），不发送。`);
        await teardownSend(conn, sendChannel, channel, gate);
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }

      // 订阅目标会话（官方 UI transport 同款 wire 参数，打开完成证据的事件流；
      // 官方发送本身只依赖握手，不依赖订阅——此处订阅只为观察轮次活动帧）。
      // 形状钉源 @ 3c5b2276：subscribe params = {workspacePath, workspaceIdentity?,
      // sessionId, base?, visibility?, retryInitialization?}
      // （packages/ui/src/v4/agentConversationTransport.ts:207-214：{...workspace,
      // sessionId, ...可选基线}；workspace = {workspacePath, workspaceIdentity?}
      // 同文件 :94-97；ZCodeAgentConversationSubscribeParams
      // packages/services/src/zcode-agent/zcodeAgent.ts:400-405）。
      // sessionId = "conversation/<sessionId>" topic 的精确后缀
      // （packages/shared/src/zcode-protocol-v4/transport.ts:113,205）。
      const subscribe = await bridgeCall("subscribeConversationV4", {
        workspacePath: bound.workspacePath,
        ...(bound.workspaceIdentity !== undefined
          ? { workspaceIdentity: bound.workspaceIdentity }
          : {}),
        sessionId: bound.taskId,
      });
      if (subscribe.kind !== "ok") {
        sendVerdict = `STOPPED:subscribe-${subscribe.kind}`;
        log(`send 停止：会话订阅未得到应答（${subscribe.kind}），不发送。`);
        await teardownSend(conn, sendChannel, channel, gate);
        await writeEvidence(
          ctx,
          oauthVerdict,
          connectVerdict,
          mappingVerdict,
          sendVerdict,
          oauthSummary,
          connectSummary,
          mappingSummary,
          sendSummary
        );
        return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
      }

      // 发送：唯一合成 marker；恰一个 rpc-frame；未知不重发。
      const marker = `[tuvis-b2-marker:${new Date().toISOString()}]`;
      const loop = new SendOnceLoop({
        sender: conn,
        identity,
        target: bound,
        markerText: marker,
        clientId,
        timeouts: { ackMs: SEND_ACK_TIMEOUT_MS, activityMs: SEND_ACTIVITY_TIMEOUT_MS },
        channelRequestId: bridgeId + 1,
        firstSeq: bridgeSeq + 1,
        firstMessageSeq: bridgeMessageSeq + 1,
        isTurnActivity: (decoded, rawJson) =>
          decoded?.headerType === RESPONSE_TYPE_EVENT_FIRE && rawJson.includes(bound.taskId),
      });
      // rpc-frame 与 rpc-frame-ack 经 handle → onRpcFrame 喂给 loop（attach 已在
      // bridge-open 前完成；ack 转发见 B2Channel.handle 的 rpc-frame-ack 分支）。
      sendChannel.onRpcFrame = (payload) => loop.onData(payload);
      const sendOutcome = await loop.run();
      sendSummary.marker = {
        prefix: "[tuvis-b2-marker:",
        issuedUtc: marker.slice(17, -1),
        synthetic: true,
      };
      sendSummary.outcome = sendOutcome;
      sendSummary.lifecycle = {
        framesSent: loop.framesSent,
        peerAcknowledged: loop.peerAcknowledged,
        duplicateInboundFrames: loop.duplicateInboundFrames,
        activityFrames: loop.activityFrames,
        commandId: loop.sentCommandId,
      };
      record(ctx, "send", "outcome", {
        outcome: sendOutcome,
        lifecycle: sendSummary.lifecycle,
        note: "恰一个 rpc-frame；重复入站帧去重；unknown 不重发（结构性无重发路径）",
      });
      log(
        `send outcome = ${sendOutcome.kind}${
          sendOutcome.kind === "completed" ? `（activityFrames=${sendOutcome.activityFrames}）` : ""
        }；framesSent=${loop.framesSent}，peerAck=${loop.peerAcknowledged}。`
      );
      sendVerdict =
        sendOutcome.kind === "completed"
          ? "COMPLETED"
          : sendOutcome.kind === "accepted"
            ? "ACCEPTED"
            : sendOutcome.kind === "rejected"
              ? "REJECTED"
              : sendOutcome.kind === "send-failed"
                ? "STOPPED:send-failed"
                : "UNKNOWN";
      recordChannelEvidence(ctx, "send", sendChannel);

      // —— s8：teardown ————————————————————————————————————————————————————————
      stage = "teardown";
      await teardownSend(conn, sendChannel, channel, gate);
      record(ctx, "teardown", "closed", {});
      log("连接已主动断开（探针侧 close；无重连路径）。JWT/一次性 token 随进程退出即弃。");
    }

    await writeEvidence(
      ctx,
      oauthVerdict,
      connectVerdict,
      mappingVerdict,
      sendVerdict,
      oauthSummary,
      connectSummary,
      mappingSummary,
      sendSummary
    );
    log(
      `oauth=${oauthVerdict} connect=${connectVerdict} mapping=${mappingVerdict} send=${sendVerdict}`
    );
    return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode: 0 };
  } catch (error) {
    // 故障围栏：任何未处理异常 → 记账 + 尽力清理 + 结构化 STOPPED verdict + 证据照常
    // 落盘（绝不裸 crash 丢证据——评审 Important 修复；exit 约定：干净停止 0，
    // 证据落盘本身失败 2）。
    const message = error instanceof Error ? error.message : String(error);
    record(ctx, "fault", "unhandled-throw", {
      stage,
      message: redactor.sanitizeText(message),
    });
    log(`探针故障（${stage} 阶段未处理异常）：${message}；清理连接并照常落证。`);
    cleanupLive();
    // 在飞阶段尚无终局 verdict → 记 STOPPED:fault-<阶段>；已定案的 verdict 不改写
    // （故障细节在 transcript 的 fault 事件条目里）。
    if (stage === "oauth" && oauthVerdict === "SKIPPED") {
      oauthVerdict = "STOPPED:fault-oauth";
    } else if (stage === "connect" && connectVerdict === "SKIPPED") {
      connectVerdict = "STOPPED:fault-connect";
    } else if (stage === "mapping" && mappingVerdict === "SKIPPED") {
      mappingVerdict = "STOPPED:fault-mapping";
    } else if (stage === "send" && sendVerdict === "SKIPPED") {
      sendVerdict = "STOPPED:fault-send";
    }
    let exitCode = 0;
    try {
      await writeEvidence(
        ctx,
        oauthVerdict,
        connectVerdict,
        mappingVerdict,
        sendVerdict,
        oauthSummary,
        connectSummary,
        mappingSummary,
        sendSummary
      );
    } catch (flushError) {
      // 证据落盘本身失败（如 fail-closed 断言拒写）：如实报告，run 不算干净完成。
      console.error(
        `[b2-probe] 证据落盘失败：${flushError instanceof Error ? flushError.message : String(flushError)}`
      );
      exitCode = 2;
    }
    log(
      `oauth=${oauthVerdict} connect=${connectVerdict} mapping=${mappingVerdict} send=${sendVerdict}`
    );
    return { oauthVerdict, connectVerdict, mappingVerdict, sendVerdict, exitCode };
  }
}

async function teardownSend(
  conn: RelayTerminalConnection,
  sendChannel: B2Channel,
  mappingChannel: B2Channel,
  gate: SingleConnectionGate
): Promise<void> {
  sendChannel.detach();
  mappingChannel.detach();
  conn.close();
  gate.release();
}

/**
 * 指称 → 精确 id：操作员指称（人读）在 listing 里**精确唯一**匹配；
 * 不唯一/不命中一律拒绝。匹配发生在内存真实值上；证据只写别名。
 */
function designateTarget(
  ctx: RunContext,
  workspaces: WorkspaceListingEntry[],
  tasks: TaskListingRow[],
  workspaceDesignation: string,
  taskDesignation: string
): { ok: true; bound: BoundTarget } | { ok: false; reason: string } {
  const wsMatches = workspaces.filter((entry) => entry.label === workspaceDesignation);
  if (wsMatches.length === 0) {
    return { ok: false, reason: "workspace-not-found" };
  }
  if (wsMatches.length > 1) {
    return { ok: false, reason: "workspace-ambiguous" };
  }
  const workspace = wsMatches[0];
  const taskMatches = tasks.filter(
    (row) =>
      row.address.workspacePath === workspace.workspacePath && row.meta?.title === taskDesignation
  );
  if (taskMatches.length === 0) {
    return { ok: false, reason: "task-not-found" };
  }
  if (taskMatches.length > 1) {
    return { ok: false, reason: "task-ambiguous" };
  }
  const row = taskMatches[0];
  const mapper = SessionTargetMapper.fromListings({
    workspaces,
    tasks,
    generation: CONNECTION_GENERATION,
  });
  const bound = mapper.resolveTarget({
    taskId: row.address.taskId,
    workspacePath: workspace.workspacePath,
  });
  record(ctx, "mapping", "designation-evidence", {
    workspaceDesignationAlias: ctx.redactor.sanitizeText(workspaceDesignation),
    taskDesignationAlias: ctx.redactor.sanitizeText(taskDesignation),
    matchedWorkspace: {
      label: workspace.label ? ctx.redactor.register("name", workspace.label) : null,
      workspacePath: ctx.redactor.register("path", workspace.workspacePath),
      kind: workspace.kind ?? null,
    },
    matchedTask: {
      taskId: ctx.redactor.register("sid", row.address.taskId),
      title: row.meta?.title ? ctx.redactor.register("name", row.meta.title) : null,
    },
    listingSizes: { workspaces: workspaces.length, tasks: tasks.length },
    note: "指称只在 designate 步用于唯一化定位；发送归因用精确 id（resolveTarget 产物）",
  });
  return { ok: true, bound };
}

function summarizePayload(outcome: AppPayloadOutcome): Record<string, unknown> {
  if (outcome.kind !== "ok") return { outcome: outcome.kind };
  const payload = outcome.payload;
  return {
    outcome: "ok",
    zcode_type: typeof payload.zcode_type === "string" ? payload.zcode_type : null,
    success: payload.success === true,
    error: typeof payload.error === "string" ? payload.error : null,
    payload: payload,
  };
}

/** 收帧证据落 events（有界 + 截断 + 脱敏；真实值经 sanitizeText 只余别名）。 */
function recordChannelEvidence(ctx: RunContext, label: string, channel: B2Channel): void {
  const messages = channel.bridgeMessages.slice(0, 64).map((message) => {
    const sanitized = ctx.redactor.sanitizeText(message);
    return sanitized.length > 40_000 ? `${sanitized.slice(0, 40_000)}…<truncated>` : sanitized;
  });
  record(ctx, label, "bridge-messages", {
    count: channel.bridgeMessages.length,
    recorded: messages.length,
    messages,
  });
  const unsolicited = channel.unsolicited
    .slice(0, 40)
    .map((payload) => redactPayload(ctx.redactor, payload));
  record(ctx, label, "unsolicited-frames", {
    count: channel.unsolicited.length,
    recorded: unsolicited.length,
    frames: unsolicited,
  });
  record(ctx, label, "rpc-frame-counts", {
    seen: channel.rpcFramesSeen.length,
    ack: channel.rpcAckCount,
    duplicate: channel.rpcDuplicateCount,
    fault: channel.rpcFaultCount,
  });
}

/** 组装并 fail-closed 落盘脱敏证据；控制台只打印私有路径。 */
async function writeEvidence(
  ctx: RunContext,
  oauthVerdict: string,
  connectVerdict: string,
  mappingVerdict: string,
  sendVerdict: string,
  oauthSummary: Record<string, unknown>,
  connectSummary: Record<string, unknown>,
  mappingSummary: Record<string, unknown>,
  sendSummary: Record<string, unknown>
): Promise<void> {
  const finishedIso = new Date().toISOString();
  const transcript = {
    runId: ctx.run.runId,
    probe: "zcode-bridge-session-probe",
    workPackage: "B2 实连半场：单连接、会话映射与单次发送闭环",
    sourceBaseline: "zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab",
    startedUtc: ctx.startedIso,
    finishedUtc: finishedIso,
    guards: PROBE_GUARDS,
    flags: {
      provider: ctx.flags.provider,
      unlock: ctx.flags.unlock,
      sendAuthorized: ctx.flags.sendAuthorized,
      oauthWaitMs: ctx.flags.oauthWaitMs,
      jwtViaStdin: ctx.flags.jwtStdin,
      sidFromCli: ctx.flags.sid !== null,
      dbPath: ctx.flags.dbPath,
      workspaceDesignationAlias: ctx.redactor.sanitizeText(ctx.flags.workspace),
      taskDesignationAlias: ctx.redactor.sanitizeText(ctx.flags.taskTitle),
    },
    oauth: oauthSummary,
    connect: connectSummary,
    mapping: mappingSummary,
    send: sendSummary,
    events: ctx.events,
  };
  const transcriptJson = JSON.stringify(ctx.redactor.sanitizeDeep(transcript), null, 2);
  const runMd = [
    `# B2 实连半场探针 run — ${ctx.run.runId}`,
    "",
    `- started: ${ctx.startedIso}`,
    `- finished: ${finishedIso}`,
    `- oauth: **${oauthVerdict}**`,
    `- connect: **${connectVerdict}**`,
    `- mapping: **${mappingVerdict}**`,
    `- send: **${sendVerdict}**`,
    `- guards: \`${JSON.stringify(PROBE_GUARDS)}\``,
    "",
    "本目录为私有证据（不进仓库）。所有 SID/JWT/token/state/授权码/链接/会话 id",
    "均以稳定别名与掩码形态记录；落盘前经 fail-closed 断言。marker 正文为合成值",
    "（[tuvis-b2-marker:<UTC>]，不含任何秘密）。发送帧恒 1；未知不重发。",
    "",
    "细节见同目录 transcript.json。",
    "",
  ].join("\n");
  writeEvidenceFiles(ctx.redactor, ctx.run.dir, [
    { name: "transcript.json", content: transcriptJson },
    { name: "run.md", content: runMd },
  ]);
  log(`证据已写入（脱敏）：${ctx.run.dir}`);
}

/** 入口：只在被直接执行时运行（被 import 时不产生副作用）。 */
export async function main(argv: readonly string[] = process.argv.slice(2)): Promise<void> {
  try {
    const outcome = await runProbe(argv);
    process.exitCode = outcome.exitCode;
  } catch (error) {
    console.error(
      `[b2-probe] 探针异常终止：${error instanceof Error ? error.message : String(error)}`
    );
    process.exitCode = 1;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  void main();
}

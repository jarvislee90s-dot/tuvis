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
//                         workspace-list-request（B1 已实连验证）；随后**主动**枚举
//                         任务索引：workspace-bridge-open（taskId 缺省）打开读面桥 →
//                         等桥 Initialize(200)（官方 ChannelClient 的 whenInitialized 门，
//                         channelClient.ts:124-128）→ EventListen onDynamicControllerFrame +
//                         subscribeControllerV4 {topic: controller/tasks-index}（channel
//                         window-controller）→ 有界收帧窗收 snapshot/deltas 任务行；收到的
//                         桥内消息按官方接收侧回累计 rpc-frame-ack（ackRelayProtocol.ts
//                         :293-304）；bridge-open 被拒（workspace-bridge-error）→
//                         bridge-open-refused 中止；
//   4. designate       —— 操作员指称（--workspace 默认 Travel、--task-title 默认 hi）：
//                         workspace 走唯一化阶梯（label 精确 → purpose=project →
//                         activeWorkspaceKey）→ task 在解析出的 workspace 内**精确唯一**
//                         匹配 title；不唯一/不命中一律中止（绝不按标题发送）；
//                         映射只绑定当前连接代际；
//   5. send（--send-authorized 显式解锁，缺省只打印 WOULD-send 即停）——
//                         workspace-bridge-open{workspaceKey, taskId} → ready → 等桥
//                         Initialize → rpc-frame 通道 hello/clientHello →
//                         EventListen(onDynamicConversationFrame, workspace) +
//                         subscribeConversationV4{workspacePath, workspaceIdentity?,
//                         sessionId=<精确 taskId>}（官方 UI transport 同序：事件监听
//                         先行、subscribe ack 后才会有 topic EventFire 帧）→
//                         sendConversationCommandV4（sendText，正文 = 唯一合成
//                         marker `[tuvis-b2-marker:<UTC>]`，模型/模式全缺省=沿用会话现
//                         值）→ 记录受理/完成/未知全生命周期（恰一个 rpc-frame，重复去
//                         重，未知不重发）→ 主动 close。
//   前置说明（源码裁决 @ 3c5b2276）：workspace-bridge-open **不要求目标 workspace 是
//   APP 当前前台**（webRemoteControlManager.ts:1000-1054 只要求目标在当前桌面窗口里；
//   bootstrap 的 mobileViewState.activeWorkspaceKey 只是手机页初始选区的导航默认，
//   web-remote-control.ts:758-783）→ 无需先切前台；APP 若拒绝会回
//   workspace-bridge-error，探针以 bridge-open-refused 中止并落 reason/error。
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
import {
  INTERACTION_COMMAND_TYPES,
  InteractionCommandLoop,
  PendingBook,
  authorizePendingResolution,
  buildPermissionAnswer,
  buildStopPayload,
  buildUserInputAnswer,
  PERMISSION_FULL_ACCESS_OPTION_ID,
  planInteractionCommands,
  type InteractionCommandType,
  type ObservedPending,
  type UserInputAnswerEntry,
} from "./interactionBridge.ts";
import { RELAY_WS_PATH, RelayTerminalConnection, requestTerminalToken } from "./relayAuth.ts";
import {
  AGENT_CHANNEL,
  AGENT_CLIENT_HELLO_METHOD,
  AGENT_HELLO_METHOD,
  ConversationTopicSubscription,
  RESPONSE_TYPE_EVENT_FIRE,
  RESPONSE_TYPE_INITIALIZE,
  ControllerTaskIndexReader,
  RpcFrameAssembler,
  SendOnceLoop,
  SessionTargetMapper,
  SingleConnectionGate,
  V4_WIRE_PROTOCOL_VERSION,
  classifyWorkspaceBridgeOpenResponse,
  decodeChannelMessage,
  dedupeWorkspaceEntries,
  encodeChannelRequest,
  encodeRpcFrame,
  encodeRpcFrameAck,
  readActiveWorkspaceKeyFromListings,
  resolveDesignatedWorkspace,
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
import { readConfigFromSnapshot, resolveCatalogFromView } from "./selectionBridge.ts";

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
   * 任务枚举阶段追加的唯一 app payload 写向 = workspace-bridge-open（打开读面订阅桥，
   * taskId 缺省，不选任务不标已读），其上只发 controller/tasks-index 的订阅/退订
   * 控制帧；send 阶段才允许真正的发送链（且必须 --send-authorized 显式解锁）。
   */
  mappingReadonly: true,
  /** 发送步硬闸：缺 --send-authorized 时映射完成后即停，绝不构造发送帧。 */
  sendRequiresExplicitFlag: true,
  /** 一次运行的发送帧数上限（SendOnceLoop 结构性恒 1；任务枚举的订阅/退订控制帧不计数）。 */
  maxSendFrames: 1,
  /**
   * connect→teardown 的声明性预算（ms；不是 wall-clock 强杀）：配对窗口 30s +
   * 2×10s 只读查询 + [10s bridge-open + 5s Initialize + 10s subscribe + 8s 收帧窗 +
   * 5s 退订]（任务枚举）+ 10s bridge-open + 5s Initialize + 10s 握手/订阅 + 20s ACK 窗
   * + 30s 活动窗 ≈ 173s。OAuth 回调等待单独有界（--wait-ms）。时限由各阶段自有界承担。
   */
  maxDurationMs: 180_000,
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
/**
 * 任务枚举收帧窗：subscribeControllerV4 ack 后收集 tasks-index snapshot/deltas 的
 * 有界窗口（B2 修复：改为主动订阅后的收帧窗，不再被动等待宿主推送）。
 */
const CAPTURE_WINDOW_MS = 8_000;
/** workspace-bridge-open 等待 ready 的有界窗口。 */
const BRIDGE_OPEN_TIMEOUT_MS = 10_000;
/**
 * 桥 ChannelServer Initialize(200) 的有界等待窗（B2 实连 124743Z 修复）：官方
 * ChannelClient 把所有请求排在 whenInitialized 之后（channelClient.ts:124-128），
 * 桥 ChannelServer 构造即发 Initialize（channelServer.ts:29-36,45-47）。
 */
const BRIDGE_INIT_TIMEOUT_MS = 15_000;
/** bridge RPC（hello/clientHello/subscribe）单请求有界窗口。 */
const BRIDGE_RPC_TIMEOUT_MS = 10_000;
/** 任务枚举的退订请求有界窗口（尽力而为；失败不影响结果）。 */
const TASK_ENUM_UNSUBSCRIBE_TIMEOUT_MS = 5_000;
/** 发送 ACK 窗口（CommandAck 应答；超窗 = unknown，绝不重发）。 */
const SEND_ACK_TIMEOUT_MS = 20_000;
/** 受理后的轮次活动观察窗。 */
const SEND_ACTIVITY_TIMEOUT_MS = 30_000;
/**
 * B3 交互半场（--interaction）：指令受理后等待宿主 pending 卡 / 前台执行 id 的有界窗。
 * 超窗 = 该能力本 run 不可用（如实记录 reason，不重试、不猜测）。
 */
const INTERACTION_PENDING_WAIT_MS = 30_000;
/** interrupt 模式下等待 control.activeWorks.foregroundExecutionId 的有界窗。 */
const INTERACTION_FOREGROUND_WAIT_MS = 30_000;

/**
 * 按运行模式派生的安全闸（I5）：B3 交互 run 实际会发 2 个命令帧（触发 + 应答/stop），
 * 若沿用 B2 的 maxSendFrames:1 与 sendRequiresExplicitFlag，证据里的闸与实际行为不符。
 * 交互模式额外允许最多 1 个 autoResolution snooze 前置帧（I4；仅当卡上
 * autoResolution 在场，见 interaction.snooze 证据），总命令帧 ≤3。
 * 时长预算：B2 链 180s + 触发 ACK 20s + 观察窗 30s + 应答 ACK 20s = 250s。
 */
export type ProbeGuardSet = {
  readonly singleConnection: true;
  readonly reconnectPolicy: "never";
  readonly stopOnKicked: true;
  readonly mappingReadonly: true;
  readonly sendRequiresExplicitFlag: boolean;
  readonly interactionRequiresExplicitFlag: boolean;
  readonly maxSendFrames: number;
  readonly maxSnoozeFrames: number;
  readonly maxDurationMs: number;
  readonly credentialsInMemoryOnly: true;
};

export function deriveProbeGuards(mode: InteractionMode | null): ProbeGuardSet {
  if (mode === null) {
    return {
      singleConnection: PROBE_GUARDS.singleConnection,
      reconnectPolicy: PROBE_GUARDS.reconnectPolicy,
      stopOnKicked: PROBE_GUARDS.stopOnKicked,
      mappingReadonly: PROBE_GUARDS.mappingReadonly,
      sendRequiresExplicitFlag: PROBE_GUARDS.sendRequiresExplicitFlag,
      interactionRequiresExplicitFlag: false,
      maxSendFrames: PROBE_GUARDS.maxSendFrames,
      maxSnoozeFrames: 0,
      maxDurationMs: PROBE_GUARDS.maxDurationMs,
      credentialsInMemoryOnly: PROBE_GUARDS.credentialsInMemoryOnly,
    };
  }
  return {
    singleConnection: PROBE_GUARDS.singleConnection,
    reconnectPolicy: PROBE_GUARDS.reconnectPolicy,
    stopOnKicked: PROBE_GUARDS.stopOnKicked,
    mappingReadonly: PROBE_GUARDS.mappingReadonly,
    // 交互模式由 --interaction-authorized 解锁；--send-authorized 不参与交互链。
    sendRequiresExplicitFlag: false,
    interactionRequiresExplicitFlag: true,
    maxSendFrames: 2,
    maxSnoozeFrames: 1,
    maxDurationMs:
      PROBE_GUARDS.maxDurationMs +
      SEND_ACK_TIMEOUT_MS +
      INTERACTION_PENDING_WAIT_MS +
      SEND_ACK_TIMEOUT_MS,
    credentialsInMemoryOnly: PROBE_GUARDS.credentialsInMemoryOnly,
  };
}
/** 交互半场指令的关键词前缀（合成 marker，正文只含 UTC 时间戳与随机短码）。 */
const INTERACTION_MARKER_PREFIX = "[tuvis-b3-probe:";
/** JWT 形状闸：三段 base64url（只判形，不解码不校验签名）。 */
const JWT_SHAPE = /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/;
/** 默认产品 DB 路径（只读打开）。 */
const DEFAULT_DB_PATH = "~/.tuvis/tuvis.db";
/** 本连接的映射代际（本探针单连接、无重连，代际恒 1；换代只发生在新的进程运行）。 */
const CONNECTION_GENERATION = 1;

/** mapping 阶段允许的 app payload 类型（B1 白名单）；枚举/send 阶段追加 bridge-open。 */
const SEND_PHASE_ALLOWED_TYPES = [...READONLY_REQUEST_TYPES, "workspace-bridge-open"] as const;
type AppPayloadRequestType = (typeof SEND_PHASE_ALLOWED_TYPES)[number];
/** 任务枚举阶段允许的 app payload 类型：mapping 只读查询 + bridge-open 打开读面桥。 */
const TASK_ENUM_ALLOWED_TYPES: readonly AppPayloadRequestType[] = [
  ...READONLY_REQUEST_TYPES,
  "workspace-bridge-open",
];

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
    title: "只读映射：bootstrap / workspace-list + 主动任务索引枚举（精确 id）",
    requires: "s4 进入 paired",
    detail:
      "白名单两个只读查询（双 listing 去重）+ workspace-bridge-open（taskId 缺省）打开读面桥" +
      "（被拒 → bridge-open-refused 中止）后等桥 Initialize → 主动订阅 controller/tasks-index" +
      "（EventListen + subscribeControllerV4，有界收帧窗）收任务行；收到的桥内消息按官方" +
      "接收侧回 rpc-frame-ack；产出 workspace 条目与 task 行（精确 taskId / workspacePath），" +
      "全程脱敏落证。",
  },
  {
    id: "s6-designate",
    title: "目标指称 → 精确 id（workspace 阶梯唯一化 + task 唯一匹配，绝不按标题发送）",
    requires: "--workspace（默认 Travel）/ --task-title（默认 hi）与 listing 精确对应",
    detail:
      "workspace：label 精确匹配 → 多候选时优先 workspacePurpose=project → 仍多候选时优先 " +
      "APP 当前打开的 workspace（bootstrap activeWorkspaceKey）→ 仍不唯一即中止；" +
      "task：在解析出的 workspace 内按 title 精确唯一匹配；不唯一/不命中 → 中止" +
      "（映射证据照常落盘）。发送授权用的是解析出的精确 id。" +
      "无需先把 APP 切到该 workspace 前台（源码裁决：bridge-open 不校验 activeWorkspaceKey）。",
  },
  {
    id: "s7-send",
    title: "单次发送（--send-authorized 显式解锁；缺省只打印 WOULD-send）",
    requires: "batch-1 授权（会话 hi 发送一条消息）+ --send-authorized",
    detail:
      "workspace-bridge-open → ready → 等桥 Initialize → hello/clientHello → " +
      "EventListen(onDynamicConversationFrame) + subscribe sessionId=<精确 taskId> → " +
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

/** B3 交互模式（--interaction=<mode>；逐能力一条链路，绝不混用）。 */
export const INTERACTION_MODES = ["approve", "reject", "answer", "interrupt", "select"] as const;
export type InteractionMode = (typeof INTERACTION_MODES)[number];

type ProbeFlags = {
  unlock: boolean;
  sendAuthorized: boolean;
  interaction: InteractionMode | null;
  interactionAuthorized: boolean;
  /** 用户显式指定应答会话中「已存在」的 pending 卡（跳过触发、不做触发后关联）。 */
  existingPending: boolean;
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
    interaction: null,
    interactionAuthorized: false,
    existingPending: false,
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
    else if (arg === "--interaction-authorized") flags.interactionAuthorized = true;
    else if (arg === "--existing-pending") flags.existingPending = true;
    else if (arg.startsWith("--interaction=")) {
      const value = arg.slice("--interaction=".length).trim();
      if (!INTERACTION_MODES.includes(value as InteractionMode)) {
        return `--interaction 只接受 ${INTERACTION_MODES.join(" | ")}，收到：${value || "(空)"}`;
      }
      flags.interaction = value as InteractionMode;
    } else if (arg === "--interaction") {
      return "--interaction 需要 =<mode> 形式，例如 --interaction=approve";
    } else if (arg === "--open") flags.open = true;
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
  lines.push("zcode-bridge-probe / session-probe — B2 会话映射与单次发送探针（B3 交互半场）");
  lines.push("");
  lines.push("用法：node scripts/zcode-bridge-probe/session-probe.ts <flags>");
  lines.push(
    "      node scripts/zcode-bridge-probe/interaction-probe.ts <approve|reject|answer|interrupt> <flags>"
  );
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
    `  --interaction=<mode>      B3 交互半场：approve | reject | answer | interrupt（缺省无）。`
  );
  lines.push(
    "                             仅与 --interaction-authorized 同时给出才执行；approve/reject 发无害"
  );
  lines.push(
    "                             文件写指令后应答宿主 pending 权限卡，answer 应答问答卡，interrupt 发"
  );
  lines.push(
    "                             长跑指令后按 activeWorks 守卫 id 精确 stop；证据落 macos-B3<run_id>。"
  );
  lines.push(
    "  --interaction-authorized  交互半场解锁（与 --interaction 成对；缺一即映射后只打印 WOULD-interact）。"
  );
  lines.push("  --workspace=<指称>        测试工作区指称（默认 Travel；listing label 精确匹配，");
  lines.push(
    "                             多候选时按 purpose=project → activeWorkspaceKey 阶梯唯一化，"
  );
  lines.push("                             仍不唯一即中止。");
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
  lines.push("  · 无需先把 APP 切到目标 workspace 前台（源码裁决：workspace-bridge-open 只要求");
  lines.push("    目标 workspace 在当前桌面窗口里，不校验 activeWorkspaceKey）；APP 拒绝时探针以");
  lines.push("    bridge-open-refused 中止并记录 APP 返回的 reason/error；");
  lines.push("  · 发送步只会向指定会话注入一条合成 marker 消息（内容仅含 UTC 时间戳），");
  lines.push("    不审批、不打断、不改模型；结果未知时绝不重发。");
  lines.push(
    "  · --interaction 交互步（需 --interaction-authorized）只做合成指令触发的单张 pending 应答或"
  );
  lines.push(
    "    有守卫 stop；pending/执行 id 未在窗内出现即如实记 capability-unavailable，不重试、不盲停。"
  );
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

export type AppPayloadOutcome =
  { kind: "ok"; payload: Record<string, unknown> } | { kind: "send-failed" } | { kind: "timeout" };

type BridgeRpcOutcome =
  { kind: "ok"; decoded: DecodedChannelMessage } | { kind: "send-failed" } | { kind: "timeout" };

/**
 * B2/B3 共用的桥通道适配器（B3 interaction-probe 复用；导出不改任何行为——
 * 单槽 onData / 白名单 payload / Initialize 门 / 接收侧回执语义见类内注释）。
 */
export class B2Channel {
  private readonly allowedTypes: readonly AppPayloadRequestType[];
  private readonly pendingApp = new Map<string, (payload: Record<string, unknown>) => void>();
  private readonly pendingBridge = new Map<number, (decoded: DecodedChannelMessage) => void>();
  private attachedConn?: { onData?: (payload: unknown) => void };
  /** 被动组装器（mapping 阶段绑定到宿主推送的 bridge 身份）。 */
  private readonly capture = new RpcFrameAssembler();
  /** 未经 request 匹配的入站 data payload（脱敏前原样持有在内存）。 */
  readonly unsolicited: unknown[] = [];
  /**
   * 被动组装完成的 Channel 消息（收帧窗内；结构化记录 JSON——header 元数据 +
     body 预览；B2 修复：不再存 lossy UTF-8 原文，二进制帧如 Initialize 不再变成
     乱码证据）。
   */
  readonly bridgeMessages: string[] = [];
  /** 收到的 rpc-frame 原始 payload（含 dataBase64；只落私有证据）。 */
  readonly rpcFramesSeen: unknown[] = [];
  /** 收到的 rpc-frame-ack 数（去重前）。 */
  rpcAckCount = 0;
  /** 接收侧回执已发送数（rpc-frame-ack；官方 AcknowledgedRelayProtocol 同款）。 */
  transportAcksSent = 0;
  /** 组装器判定的重复/故障计数。 */
  rpcDuplicateCount = 0;
  rpcFaultCount = 0;
  /** send 阶段钩子：每条入站 rpc-frame 转交 SendOnceLoop（onData 单槽的唯一出口）。 */
  onRpcFrame?: (payload: unknown) => void;
  /** 桥 ChannelServer Initialize(200) 是否已观察到。 */
  private initializeObserved = false;
  private initializeWaiters: Array<(observed: boolean) => void> = [];
  /**
   * 接收侧回执状态：bridge-ready 后由调用方启用（B2Channel 是该连接 rpc-frame 的
   * 唯一组装/回执点——枚举 reader 与 SendOnceLoop 都不重复回执，避免双份 ack）。
   */
  private transportAck: {
    identity: RpcFrameIdentity;
    sender: { sendData(payload: unknown): boolean };
  } | null = null;

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

  /**
   * 启用接收侧回执（官方接收侧对每条完整组装的入站消息回累计 rpc-frame-ack，
   * ackRelayProtocol.ts:293-304；不回执 → 对端按重放期限反复重发（B2 实连 124743Z
   * 的 3×Initialize）并最终把桥降级）。identity 必须 = workspace-bridge-open 请求
   * 声明并被 ready 回显的字段（见 classifyWorkspaceBridgeOpenResponse 注）。
   */
  enableTransportAcks(args: {
    identity: RpcFrameIdentity;
    sender: { sendData(payload: unknown): boolean };
  }): void {
    this.transportAck = { identity: args.identity, sender: args.sender };
  }

  /**
   * 有界等待桥 Initialize(200)（官方 ChannelClient 的 whenInitialized 门，
   * channelClient.ts:124-128——Initialize 到达前不发任何 Channel 请求）。
   * 超窗返回 false（结构性失败，由调用方中止）。
   */
  waitForBridgeInitialize(timeoutMs: number): Promise<boolean> {
    if (this.initializeObserved) return Promise.resolve(true);
    return new Promise<boolean>((resolve) => {
      const waiter = (observed: boolean) => {
        clearTimeout(timer);
        resolve(observed);
      };
      const timer = setTimeout(
        () => {
          this.initializeWaiters = this.initializeWaiters.filter((entry) => entry !== waiter);
          resolve(false);
        },
        Math.max(1, timeoutMs)
      );
      this.initializeWaiters.push(waiter);
    });
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

  /** send 阶段唯一追加的写向：workspace-bridge-open（仍过白名单闸）。
   * taskId 可选（web-remote-control.ts:435-447 `taskId?`）；任务枚举期省略。 */
  openWorkspaceBridge(
    sender: { sendData(payload: unknown): boolean },
    payload: { requestId: string; bridgeSessionId: string; workspaceKey: string; taskId?: string },
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
      /** 目标 channel（默认 zcode-agent；B4 目录读取用 model-selection）。 */
      channel?: string;
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
      channel: args.channel ?? AGENT_CHANNEL,
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
        if (decoded && decoded.headerType === RESPONSE_TYPE_INITIALIZE) {
          // 桥 ChannelServer 就绪信号（channelServer.ts:29-36,45-47）：解锁等待方。
          if (!this.initializeObserved) {
            this.initializeObserved = true;
            const waiters = this.initializeWaiters.splice(0);
            for (const waiter of waiters) waiter(true);
          }
        }
        this.recordBridgeMessage(decoded, event.bytes);
        // 接收侧回执（官方 ackRelayProtocol.ts:293-304：每条完整组装的入站消息回
        // 累计 rpc-frame-ack；重复帧对齐 :289-291 的 lastCompleted 语义）。回执是
        // 读面传输控制帧，不计入任何发送帧数。
        if (this.transportAck) {
          const ack = encodeRpcFrameAck({
            identity: this.transportAck.identity,
            ackMessageSeq: event.messageSeq,
          });
          if (this.transportAck.sender.sendData(ack)) this.transportAcksSent += 1;
        }
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

  /**
   * 桥 Channel 消息证据：结构化记录（header 元数据 + body 预览），超过上限丢弃。
   * B2 修复：此前存 `Buffer→utf8` 的 lossy 原文，二进制帧（Initialize =
   * `04 01 06 C8 01 00`，channelServer.ts:45-47）在证据里变成乱码、且因
   * decodeChannelMessage 旧门（Array ≥ 2）解不出而完全不可读。
   */
  private recordBridgeMessage(decoded: DecodedChannelMessage | null, bytes: Uint8Array): void {
    if (this.bridgeMessages.length >= 64) return;
    const entry = {
      kind: "channel-message",
      bytes: bytes.byteLength,
      headerType: decoded?.headerType ?? null,
      id: decoded?.id ?? null,
      channelName: decoded?.channelName ?? null,
      method: decoded?.method ?? null,
      initialize: decoded?.headerType === RESPONSE_TYPE_INITIALIZE,
      bodyPreview: Buffer.from(bytes).toString("utf8").slice(0, 2048),
    };
    this.bridgeMessages.push(JSON.stringify(entry));
  }
}

// ---------------------------------------------------------------------------
// listing 采集：workspace 条目与 task 行（精确 id）从真实帧里结构化收割
// ---------------------------------------------------------------------------

/** listing 采集：workspace 条目与 task 行（精确 id）从真实帧里结构化收割
 * （B3 interaction-probe 复用；导出不改任何行为）。 */

export function harvestTaskRows(value: unknown, out: TaskListingRow[]): void {
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

export function harvestWorkspaceEntries(
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
        ...(typeof record.workspacePurpose === "string"
          ? { workspacePurpose: record.workspacePurpose }
          : {}),
      });
    }
  }
}

export function dedupeRows(rows: TaskListingRow[]): TaskListingRow[] {
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

/**
 * 目标锁定行的 stdout 文案（2026-10-24 收官修复）：此前该行把**真实** taskId /
 * workspaceKey 直接打上 stdout——证据文件经 SecretRedactor fail-closed 脱敏，但
 * stdout 不在落盘闸内（run macos-B220261024T132316Z-prItvw 即漏）。修复 = 真实值
 * 一律先经 redactor 登记取别名再拼行（mapping 阶段已登记过同一值 → 复用同一别名）；
 * 人类可读的 verdict 措辞保持不变。
 */
export function formatTargetLockedLine(
  redactor: SecretRedactor,
  target: { taskId: string; workspaceKey: string; generation: number }
): string {
  const taskIdAlias = redactor.register("sid", target.taskId) ?? "<unregistered>";
  const workspaceAlias = redactor.register("path", target.workspaceKey) ?? "<unregistered>";
  return `目标已锁定（别名）：taskId=${taskIdAlias} workspace=${workspaceAlias}（generation ${target.generation}）。`;
}

/**
 * B3 交互半场的合成指令（唯一 marker 前缀；正文只含 UTC 时间戳与目标文件名，
 * 不含任何真实会话内容/凭据）。措辞取「可观察、无害、可回滚」的最小面：
 *   - approve/reject：一次临时文件创建（触发权限卡；路径在当前工作区，用户可删）；
 *   - answer：请宿主用提问卡问一个二选一（观察 AskUserQuestion 卡与应答闭环）；
 *   - interrupt：顺序逐行写文件的长任务（为精确停提供前台执行 id 观察窗）。
 */
function buildInteractionInstruction(mode: InteractionMode): string {
  const tag = `${INTERACTION_MARKER_PREFIX}${new Date().toISOString()}]`;
  switch (mode) {
    case "approve":
    case "reject":
      // 文件写入在多数会话里免批（B3 实跑 230311Z 即直接建文件成功）；要触发
      // permission 卡，用一条无害只读的 shell 命令（exec 审批最常见）。
      return `${tag} 请在终端执行命令：pwd && echo tuvis-b3-shell-test。这是一条只读无害命令，执行后把输出原样告诉我。`;
    case "answer":
      return `${tag} 请用提问卡问我一个单选题，问题原文用「继续还是暂停？」，提供两个选项：继续 / 暂停。等我回答后再继续。`;
    case "interrupt":
      return `${tag} 请顺序执行一个耗时任务：从 1 数到 80，每一步把当前数字单独写一行追加到临时文件 tuvis-b3-interrupt-test.txt，逐行进行，不要合并、不要跳步，完成后简短总结。`;
    case "select":
      // select 模式不触发任何会话指令（只切档/恢复）；保留穷尽性。
      return `${tag} selection-mode has no trigger instruction`;
  }
}

/**
 * 权限卡选项 id 收集：**只**读 payload.options（snapshot.ts:235-262
 * permissionRequestPayloadSchema.options 是唯一选项面），不再对整份 payload 做深度
 * 扫描——深度扫描会把 detail/display 里的同名字符串误当选项（I3）。
 */
export function collectOptionIdsFromOptions(options: unknown): string[] {
  if (!Array.isArray(options)) return [];
  const found: string[] = [];
  for (const option of options) {
    if (!option || typeof option !== "object") continue;
    const record = option as { optionId?: unknown };
    if (typeof record.optionId === "string" && record.optionId.length > 0) {
      found.push(record.optionId);
    }
  }
  return [...new Set(found)];
}

/**
 * 权限卡"常驻授权"判定：snapshot.ts:235-240 permissionOptionSchema.kind 闭集 =
 * allowOnce | allowAlways | deny | custom；allowAlways 是常驻授权，探针绝不选。
 * fullAccess 另由 optionId 硬闸（snapshot.ts:234 + buildPermissionAnswer 断言）。
 */
export function isStandingPermissionGrant(kind: unknown): boolean {
  return kind === "allowAlways";
}

/**
 * 权限卡选项选择（I3）：只从卡上 payload.options 的显式 optionId 里挑——
 *   approve → kind === "allowOnce"（一次性放行；常驻授权 allowAlways / custom 一律不选）；
 *   reject  → kind === "deny"；
 * fullAccess 特权项（optionId = "fullAccess"，snapshot.ts:234）无论 kind 都硬闸排除。
 * 没有唯一明确的 once/deny 项 → null（调用方记 UNAVAILABLE:permission-option-unresolved，
 * 绝不按字符串相似度猜 optionId）。
 */
export function pickPermissionOption(
  pending: ObservedPending,
  mode: "approve" | "reject"
): string | null {
  const options = (pending.payload as { options?: unknown }).options;
  if (!Array.isArray(options)) return null;
  const wantedKind = mode === "approve" ? "allowOnce" : "deny";
  for (const option of options) {
    if (!option || typeof option !== "object") continue;
    const record = option as { optionId?: unknown; kind?: unknown };
    if (typeof record.optionId !== "string" || record.optionId.length === 0) continue;
    if (record.optionId === PERMISSION_FULL_ACCESS_OPTION_ID) continue;
    if (isStandingPermissionGrant(record.kind)) continue;
    if (record.kind !== wantedKind) continue;
    return record.optionId;
  }
  return null;
}

/**
 * 问答卡可答性（C2）：只有 AskUserQuestion 投影出的 userInput 卡才可进入 answer 链。
 * 判别式（product-projection.ts @ 3c5b2276）：
 *   - AskUserQuestion：permission_requested → kind:"userInput" 且
 *     payload.toolName === "AskUserQuestion"（:3391-3409；isAskUserQuestionToolName
 *     :5793-5795）；
 *   - ExitPlanMode 计划审批**同样**投影成 kind:"userInput"（:3440-3459，
 *     schema.interaction = "plan_approval"，question = reason，options[0].value =
 *     "approve"）——它不是问答，绝不能当问答应答；
 *   - MCP elicitation 也是 userInput（:3411-3438，schema.interaction =
 *     MCP_ELICITATION_INTERACTION）；无可识别工具的卡一律不支持。
 */
export function isAnswerableUserInput(pending: ObservedPending): boolean {
  if (pending.kind !== "userInput") return false;
  const payload = pending.payload;
  if (payload.toolName !== "AskUserQuestion") return false;
  const schema = payload.schema;
  if (
    schema &&
    typeof schema === "object" &&
    (schema as { interaction?: unknown }).interaction === "plan_approval"
  ) {
    return false;
  }
  return true;
}

/**
 * 问答卡应答条目：题干全文 + 首个选项 value（原值回传）。调用方须先过
 * isAnswerableUserInput（否则 plan approval 也可被当成问答）。形状不可解 = null
 * （调用方记 UNAVAILABLE:answer-shape-unresolved，绝不虚构题干/选项）。
 */
export function pickUserInputEntries(pending: ObservedPending): UserInputAnswerEntry[] | null {
  const questions = (pending.payload as { questions?: unknown }).questions;
  if (!Array.isArray(questions) || questions.length === 0) return null;
  const entries: UserInputAnswerEntry[] = [];
  for (const question of questions) {
    if (!question || typeof question !== "object") return null;
    const record = question as Record<string, unknown>;
    const questionText =
      typeof record.question === "string"
        ? record.question
        : typeof record.text === "string"
          ? record.text
          : null;
    const options = Array.isArray(record.options) ? record.options : [];
    const values = options
      .map((option) =>
        option && typeof option === "object" ? (option as { value?: unknown }).value : null
      )
      .filter((value): value is string => typeof value === "string" && value.length > 0);
    if (!questionText || values.length === 0) return null;
    entries.push({
      questionText,
      selectedValues: [values[0]!],
      multiSelect: record.multiSelect === true,
    });
  }
  return entries;
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
  const run = createEvidenceRunDir(undefined, parsed.interaction !== null ? "B3" : "B2");
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

      // 结构化收割（真实值只在内存；登记别名后只落别名证据）。
      const workspacesRaw: WorkspaceListingEntry[] = [];
      harvestWorkspaceEntries(
        [
          bootstrap.kind === "ok" ? bootstrap.payload : null,
          workspaceList.kind === "ok" ? workspaceList.payload : null,
        ],
        workspacesRaw
      );
      // B2 修复：bootstrap 与 workspace-list 携带同一份 workspaces[]（实连
      // macos-B220261024T121929Z-SnWWVA：14×2=28），不去重则任何 label 精确匹配
      // 至少翻倍、「精确唯一」永假（实连即在 workspace-ambiguous fail-closed）。
      const workspaces = dedupeWorkspaceEntries(workspacesRaw);
      // APP 当前打开的 workspace（只读自 bootstrap response；阶梯第三级判据）。
      const activeWorkspaceKey = readActiveWorkspaceKeyFromListings({
        bootstrap: bootstrap.kind === "ok" ? bootstrap.payload : null,
        workspaceList: workspaceList.kind === "ok" ? workspaceList.payload : null,
      });
      for (const entry of workspaces) {
        redactor.register("path", entry.workspacePath);
        if (entry.workspaceIdentity) redactor.register("sid", entry.workspaceIdentity);
        if (entry.remoteSessionId) redactor.register("sid", entry.remoteSessionId);
        if (entry.label) redactor.register("name", entry.label);
      }
      if (activeWorkspaceKey) redactor.register("path", activeWorkspaceKey);
      mappingSummary.listings = {
        bootstrapOutcome: bootstrap.kind,
        workspaceListOutcome: workspaceList.kind,
        workspaceEntriesRaw: workspacesRaw.length,
        workspaceEntries: workspaces.length,
        activeWorkspaceKnown: activeWorkspaceKey !== null,
        bridgeMessages: channel.bridgeMessages.length,
        rpcFramesSeen: channel.rpcFramesSeen.length,
        rpcAckCount: channel.rpcAckCount,
        rpcDuplicateCount: channel.rpcDuplicateCount,
        rpcFaultCount: channel.rpcFaultCount,
      };
      record(ctx, "mapping", "harvest", mappingSummary.listings);
      recordChannelEvidence(ctx, "mapping", channel);
      log(
        `映射输入：workspace 条目 ×${workspaces.length}（原始 ×${workspacesRaw.length}，双 listing 去重后）；` +
          `activeWorkspaceKey ${activeWorkspaceKey ? "已知" : "未知"}。`
      );

      // —— s6a：workspace 指称唯一化（label → purpose=project → activeWorkspaceKey） ——
      const wsDesignation = resolveDesignatedWorkspace({
        workspaces,
        label: parsed.workspace,
        activeWorkspaceKey,
      });
      record(ctx, "mapping", "designation-workspace-evidence", {
        workspaceDesignationAlias: redactor.sanitizeText(parsed.workspace),
        evidence: wsDesignation.evidence,
        activeWorkspaceKeyAlias: activeWorkspaceKey
          ? redactor.sanitizeText(activeWorkspaceKey)
          : null,
        resolvedWorkspace: wsDesignation.ok
          ? {
              label: wsDesignation.entry.label
                ? redactor.register("name", wsDesignation.entry.label)
                : null,
              workspacePath: redactor.register("path", wsDesignation.entry.workspacePath),
              kind: wsDesignation.entry.kind ?? null,
              workspacePurpose: wsDesignation.entry.workspacePurpose ?? null,
            }
          : null,
        note: "阶梯证据只含 sanitized 计数/布尔/步骤标签，绝不含真实值",
      });
      if (!wsDesignation.ok) {
        mappingVerdict = `ABORTED:designate-${wsDesignation.reason}`;
        record(ctx, "mapping", "designate-failed", {
          reason: wsDesignation.reason,
          stage: "workspace",
          workspaceDesignation: redactor.sanitizeText(parsed.workspace),
        });
        log(
          `映射中止：workspace ${wsDesignation.reason}（未发送任何写向；未匹配的指称不入证据正文）。`
        );
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
      const workspaceEntry = wsDesignation.entry;

      // —— s5b：任务索引主动枚举（B2 修复）————————————————————————————————————
      // 被动收帧窗实连 0 条 rpc-frame → 任务行必须主动拉。方法（行号 @ 3c5b2276）：
      // workspace-bridge-open（taskId 可选，web-remote-control.ts:435-447，枚举期省略）
      // → 就绪身份 → channel "window-controller"（channels.ts:112；只在 workspace
      // bridge relay 通道发布，remoteServiceAccess.ts:181-185 + websocket.ts:102-108 +
      // webRemoteControlWorkspaceBridgeSession.ts:39-74）上：等桥 Initialize(200)
      // （官方 ChannelClient 的 whenInitialized 门，channelClient.ts:124-128）→
      // EventListen(102) "onDynamicControllerFrame"（windowController.ts:78；
      // proxy-channel.ts:126-127；channelClient.ts:150-177,179-194；官方先监听后订阅
      // windowControllerTaskListRegistry.ts:145,220-224）+ subscribeControllerV4
      // {topic:"controller/tasks-index", visibility:"foreground"}
      // （windowController.ts:75；controller.ts:258-271；registry 同款 visibility
      // windowControllerTaskListRegistry.ts:219-232；ack transport.ts:125-131 +
      // controller.ts:273-274）→ 有界收帧窗收 snapshot/deltas 任务行（controller.ts
      // :251-256,155-164,187-191,93-123）。全部为读面控制帧，无会话发送帧。
      const enumBridgeSessionId = `b2-bridge-${crypto.randomUUID()}`;
      redactor.register("sid", enumBridgeSessionId);
      const enumChannel = new B2Channel({ allowedTypes: TASK_ENUM_ALLOWED_TYPES });
      liveChannels.push(enumChannel);
      enumChannel.attach(conn);
      const enumBridgeOpen = await enumChannel.openWorkspaceBridge(
        conn,
        {
          requestId: `b2-bridge-open-${crypto.randomBytes(6).toString("hex")}`,
          bridgeSessionId: enumBridgeSessionId,
          // workspaceKey 口径 = workspaceIdentity?.trim() || workspacePath
          // （web-remote-control.ts:744-747）。
          workspaceKey: workspaceEntry.workspaceIdentity?.trim() || workspaceEntry.workspacePath,
        },
        BRIDGE_OPEN_TIMEOUT_MS
      );
      record(ctx, "mapping", "tasks-bridge-open", summarizePayload(enumBridgeOpen));
      if (enumBridgeOpen.kind !== "ok") {
        mappingVerdict = `ABORTED:designate-task-bridge-open-${enumBridgeOpen.kind}`;
        record(ctx, "mapping", "designate-failed", {
          reason: mappingVerdict,
          stage: "task-enumeration",
        });
        log(`映射中止：任务枚举桥未就绪（${enumBridgeOpen.kind}）（未发送任何写向）。`);
        conn.close();
        enumChannel.detach();
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
      // ready 应答分类（webRemoteControlManager.ts:1150-1192）：成功 = workspace-
      // bridge-ready（无 success 字段）；拒绝 = workspace-bridge-error（reason/error）。
      const enumOpenClass = classifyWorkspaceBridgeOpenResponse(enumBridgeOpen.payload);
      if (enumOpenClass.kind !== "ready") {
        mappingVerdict =
          enumOpenClass.kind === "refused"
            ? "ABORTED:bridge-open-refused"
            : "ABORTED:bridge-open-unknown-shape";
        record(ctx, "mapping", "tasks-bridge-open-refused", {
          classification: enumOpenClass.kind,
          reason:
            enumOpenClass.kind === "refused" && enumOpenClass.reason !== null
              ? redactor.sanitizeText(enumOpenClass.reason)
              : null,
          error:
            enumOpenClass.kind === "refused" && enumOpenClass.error !== null
              ? redactor.sanitizeText(enumOpenClass.error)
              : null,
          note: "APP 明确拒绝或应答形状不可解；ready 本身不携带 success 字段（web-remote-control.ts:449-456）",
        });
        log(`映射中止：workspace-bridge-open 被拒绝（${mappingVerdict}）（未发送任何写向）。`);
        conn.close();
        enumChannel.detach();
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
      // 帧身份 = ready 回显（只含请求声明过的字段）；绝不虚构默认值——APP 接收侧
      // rawIdentityMatches 严格相等（含 undefined，ackRelayTypes.ts:65-76），多余字段
      // 会被静默丢帧且不回 ack（B2 实连 124743Z subscribe-timeout 根因）。
      const enumReady = {
        identity: enumOpenClass.identity,
        bridgeSessionId: enumOpenClass.bridgeSessionId,
        bridgeKind: enumOpenClass.bridgeKind,
      };
      redactor.register("sid", enumReady.bridgeSessionId);
      record(ctx, "mapping", "tasks-bridge-ready", {
        bridgeSessionId: enumReady.bridgeSessionId,
        bridgeGeneration: enumReady.identity.bridgeGeneration ?? null,
        bridgeKind: enumReady.bridgeKind,
      });
      // 接收侧回执启用（ackRelayProtocol.ts:293-304）：不回执 → 对端反复重放并最终
      // 降级桥（B2 实连 124743Z 的 3×Initialize 即未回执的重放）。
      enumChannel.enableTransportAcks({ identity: enumReady.identity, sender: conn });
      const reader = new ControllerTaskIndexReader({
        sender: conn,
        identity: enumReady.identity,
        timeouts: {
          initializeMs: BRIDGE_INIT_TIMEOUT_MS,
          subscribeMs: BRIDGE_RPC_TIMEOUT_MS,
          windowMs: CAPTURE_WINDOW_MS,
          unsubscribeMs: TASK_ENUM_UNSUBSCRIBE_TIMEOUT_MS,
        },
      });
      enumChannel.onRpcFrame = (payload) => reader.onData(payload);
      log(`主动任务枚举：EventListen + subscribeControllerV4（收帧窗 ${CAPTURE_WINDOW_MS} ms）……`);
      const enumResult = await reader.run();
      recordChannelEvidence(ctx, "tasks", enumChannel);
      // 枚举收帧窗已闭：摘除枚举通道（conn.onData 单槽让位给 send 阶段的通道；
      // detach 幂等，后续 abort 路径重复调用为 no-op）。
      enumChannel.detach();
      mappingSummary.taskEnumeration = {
        outcome: enumResult.kind,
        ackMode: enumResult.kind === "ok" ? enumResult.ackMode : null,
        ...enumResult.counters,
      };
      record(ctx, "mapping", "task-enumeration", mappingSummary.taskEnumeration);
      log(
        `任务枚举 = ${enumResult.kind}（rows=${enumResult.kind === "ok" ? enumResult.rows.length : 0}，` +
          `snapshotFrames=${enumResult.counters.snapshotFrames}，deltaFrames=${enumResult.counters.deltaFrames}）。`
      );
      if (enumResult.kind !== "ok") {
        mappingVerdict = `ABORTED:designate-task-${enumResult.kind}`;
        record(ctx, "mapping", "designate-failed", {
          reason: enumResult.kind,
          stage: "task-enumeration",
        });
        log(`映射中止：任务枚举失败（${enumResult.kind}）（未发送任何写向）。`);
        conn.close();
        enumChannel.detach();
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
      // 任务行 = 主动枚举产物 + mapping 通道被动桥帧兜底（同键去重；枚举行是权威，
      // 兜底只收割恰好以纯 JSON 形态出现在被动消息里的任务行——桥内 Channel 消息是
      // 二进制序列化，证据以结构化记录存储，不会贡献任务行）。
      const passiveRows: TaskListingRow[] = [];
      for (const message of channel.bridgeMessages) {
        try {
          harvestTaskRows(JSON.parse(message) as unknown, passiveRows);
        } catch {
          // 非 JSON 消息跳过（计数即可）。
        }
      }
      const tasks = dedupeRows([...enumResult.rows, ...passiveRows]);
      for (const row of tasks) {
        redactor.register("path", row.address.workspacePath);
        redactor.register("sid", row.address.taskId);
        if (row.meta?.title) redactor.register("name", row.meta.title);
      }
      mappingSummary.listings = {
        ...(mappingSummary.listings as Record<string, unknown>),
        taskRows: tasks.length,
        taskRowsFromEnumeration: enumResult.rows.length,
      };

      // —— s6b：task 指称在解析出的 workspace 内精确唯一匹配（绝不按标题发送） ——
      const taskMatches = tasks.filter(
        (row) =>
          row.address.workspacePath === workspaceEntry.workspacePath &&
          row.meta?.title === parsed.taskTitle
      );
      if (taskMatches.length === 0 || taskMatches.length > 1) {
        mappingVerdict = `ABORTED:designate-task-${taskMatches.length === 0 ? "not-found" : "ambiguous"}`;
        record(ctx, "mapping", "designate-failed", {
          reason: taskMatches.length === 0 ? "task-not-found" : "task-ambiguous",
          stage: "task",
          taskDesignation: redactor.sanitizeText(parsed.taskTitle),
          workspacePathAlias: redactor.register("path", workspaceEntry.workspacePath),
          candidateRows: taskMatches.length,
        });
        log(
          `映射中止：task ${taskMatches.length === 0 ? "task-not-found" : "task-ambiguous"}（未发送任何写向）。`
        );
        conn.close();
        enumChannel.detach();
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
      const taskRow = taskMatches[0];
      const mapper = SessionTargetMapper.fromListings({
        workspaces,
        tasks,
        generation: CONNECTION_GENERATION,
      });
      let bound: BoundTarget;
      try {
        bound = mapper.resolveTarget({
          taskId: taskRow.address.taskId,
          workspacePath: workspaceEntry.workspacePath,
        });
        mapper.authorizeSend(bound, CONNECTION_GENERATION);
      } catch (error) {
        mappingVerdict = "ABORTED:generation";
        record(ctx, "mapping", "generation-check-failed", {
          reason: error instanceof Error ? error.message : String(error),
        });
        conn.close();
        enumChannel.detach();
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
      record(ctx, "mapping", "designation-evidence", {
        workspaceDesignationAlias: redactor.sanitizeText(parsed.workspace),
        taskDesignationAlias: redactor.sanitizeText(parsed.taskTitle),
        workspaceEvidence: wsDesignation.evidence,
        activeWorkspaceConfirmed: wsDesignation.evidence.activeWorkspaceConfirmed,
        matchedWorkspace: {
          label: workspaceEntry.label ? redactor.register("name", workspaceEntry.label) : null,
          workspacePath: redactor.register("path", workspaceEntry.workspacePath),
          kind: workspaceEntry.kind ?? null,
          workspacePurpose: workspaceEntry.workspacePurpose ?? null,
        },
        matchedTask: {
          taskId: redactor.register("sid", taskRow.address.taskId),
          title: taskRow.meta?.title ? redactor.register("name", taskRow.meta.title) : null,
        },
        listingSizes: { workspaces: workspaces.length, tasks: tasks.length },
        note: "指称只在 designate 步用于唯一化定位；发送归因用精确 id（resolveTarget 产物）",
      });
      log(formatTargetLockedLine(redactor, bound));

      // —— s7：send / B3 交互半场（两把锁各自独立：send 只认 --send-authorized，
      // interaction 只认 --interaction + --interaction-authorized；缺锁一律映射后即停）
      stage = "send";
      const interactionMode = parsed.interaction;
      if (interactionMode !== null && !parsed.interactionAuthorized) {
        sendVerdict = "MAPPED-NOT-SENT";
        sendSummary.attempted = false;
        sendSummary.reason = "missing --interaction-authorized";
        sendSummary.interaction = {
          mode: interactionMode,
          attempted: false,
          reason: "missing --interaction-authorized",
          chain:
            "workspace-bridge-open{workspaceKey,taskId} → ready → 等桥 Initialize → hello/clientHello → " +
            "EventListen(onDynamicConversationFrame,workspace) + subscribeConversationV4 → " +
            "一条合成指令（sendConversationCommandV4）→ 有界等宿主 pending/" +
            "前台执行 id → resolveInteraction 或带守卫 stop（各恰一个 rpc-frame）",
          note: "无授权 = 不构造任何交互帧；映射与 target lock 证据照常落盘。",
        };
        record(ctx, "interaction", "would-interact", sendSummary.interaction);
        log(
          `未提供 --interaction-authorized：映射完成即停（--interaction=${interactionMode} 未执行）。`
        );
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
      if (interactionMode === null && !parsed.sendAuthorized) {
        sendVerdict = "MAPPED-NOT-SENT";
        sendSummary.attempted = false;
        sendSummary.reason = "missing --send-authorized";
        const marker = `[tuvis-b2-marker:${new Date().toISOString()}]`;
        sendSummary.wouldSend = {
          chain:
            "workspace-bridge-open{workspaceKey,taskId} → ready → 等桥 Initialize → " +
            "rpc-frame 通道 helloConversationV4 → initializeConversationV4(clientHello) → " +
            "EventListen(onDynamicConversationFrame,workspace) + " +
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
      // web-remote-control.ts:435-447）；本请求不带 generation/recoveryId → ready 回显
      // 与后续 rpc-frame 身份同样缺席（manager.ts:1156-1165 只回显请求里存在的字段；
      // APP 接收侧严格相等判帧身份，ackRelayTypes.ts:65-76）。
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
      // ready 分类：拒绝（workspace-bridge-error，manager.ts:1175-1191）或形状不可解
      // → 带脱敏 reason/error 的独立中止 verdict；绝不带着可疑身份进入发送链。
      const sendOpenClass = classifyWorkspaceBridgeOpenResponse(bridgeOpen.payload);
      if (sendOpenClass.kind !== "ready") {
        sendVerdict =
          sendOpenClass.kind === "refused"
            ? "STOPPED:bridge-open-refused"
            : "STOPPED:bridge-open-unknown-shape";
        record(ctx, "send", "workspace-bridge-open-refused", {
          classification: sendOpenClass.kind,
          reason:
            sendOpenClass.kind === "refused" && sendOpenClass.reason !== null
              ? redactor.sanitizeText(sendOpenClass.reason)
              : null,
          error:
            sendOpenClass.kind === "refused" && sendOpenClass.error !== null
              ? redactor.sanitizeText(sendOpenClass.error)
              : null,
          note: "APP 明确拒绝或应答形状不可解；ready 本身不携带 success 字段（web-remote-control.ts:449-456）",
        });
        log(`send 停止：workspace-bridge-open 被拒绝（${sendVerdict}），未发送任何会话帧。`);
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
      const identity: RpcFrameIdentity = sendOpenClass.identity;
      const resolvedBridgeSessionId = sendOpenClass.bridgeSessionId;
      redactor.register("sid", resolvedBridgeSessionId);
      record(ctx, "send", "workspace-bridge-ready", {
        bridgeSessionId: resolvedBridgeSessionId,
        bridgeGeneration: identity.bridgeGeneration ?? null,
        kind: sendOpenClass.bridgeKind,
      });
      log("workspace bridge 已就绪。");
      // 接收侧回执 + Initialize 门（B2 实连 124743Z 修复）：官方 ChannelClient 把
      // hello 等一切请求排在 whenInitialized 之后（channelClient.ts:124-128）；
      // 不回执 → 对端重放并最终降级桥（ackRelayProtocol.ts:293-304 + replay 期限）。
      sendChannel.enableTransportAcks({ identity, sender: conn });
      const bridgeInitialized = await sendChannel.waitForBridgeInitialize(BRIDGE_INIT_TIMEOUT_MS);
      record(ctx, "send", "bridge-initialize", {
        observed: bridgeInitialized,
        transportAcksSent: sendChannel.transportAcksSent,
      });
      if (!bridgeInitialized) {
        sendVerdict = "STOPPED:bridge-init-timeout";
        log("send 停止：桥 Initialize 未在窗口内到达（channelClient.ts:124-128 门），不发送。");
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
      log(
        `workspace bridge Initialize 已到（transportAcksSent=${sendChannel.transportAcksSent}）。`
      );

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

      // 目标会话 topic 订阅（B3 实连修复）：conversation 帧只经 workspace 级动态事件
      // onDynamicConversationFrame 下发——必须先 EventListen(onDynamicConversationFrame,
      // workspace) 再 subscribeConversationV4（官方 transport 同序，
      // agentConversationTransport.ts:187-259,539-554；dynamic event = EventListen+arg，
      // proxy-channel.ts:126-127,61-74）。只发 subscribe RPC 时桥只回 request/response，
      // topic/unsolicited 帧恒 0（B3 实连 230311Z/230432Z 的根因）。
      const book = new PendingBook({
        sessionId: bound.taskId,
        generation: CONNECTION_GENERATION,
      });
      /** B4：最近一次会话快照（topic snapshot/deltas 合并视图；select 模式读配置）。 */
      let latestSnapshot: Record<string, unknown> | null = null;
      // 单一入站帧汇（C1）：trigger 运行期间也必须收到 rpc-frame-ack / CommandAck，
      // 否则 trigger 永远 unknown、观察窗还会晚 20s 才开始。currentLoop 在 trigger
      // run 之前指向 trigger，resolution run 之前换成 resolution；topic 订阅器照旧
      // 并行消费同一批帧（pending 卡来自 conversation topic 的 EventFire）。
      let currentLoop: { onData(payload: unknown): void } | null = null;
      const topicSubscription = new ConversationTopicSubscription({
        sender: conn,
        identity,
        sessionId: bound.taskId,
        workspace: {
          workspacePath: bound.workspacePath,
          ...(bound.workspaceIdentity !== undefined
            ? { workspaceIdentity: bound.workspaceIdentity }
            : {}),
        },
        subscribeMs: BRIDGE_RPC_TIMEOUT_MS,
        firstRequestId: bridgeId + 1,
        firstSeq: bridgeSeq + 1,
        firstMessageSeq: bridgeMessageSeq + 1,
        onFrame: (frame) => {
          book.acceptTopicFrame(frame);
          // B4：会话快照/增量同时供 select 模式读 config/revision/availability
          // （snapshot.config.{provider,model,thought,thoughtLevels}，snapshot.ts:506；
          //  deltas patch 键级整体替换，delta.ts:2-9,49）。
          const framePayload = (frame as { payload?: unknown }).payload;
          if (!framePayload || typeof framePayload !== "object") return;
          const kind = (framePayload as { kind?: unknown }).kind;
          if (kind === "snapshot") {
            const snap = (framePayload as { snapshot?: unknown }).snapshot;
            if (snap && typeof snap === "object") latestSnapshot = snap as Record<string, unknown>;
          } else if (kind === "deltas") {
            const deltas = (framePayload as { deltas?: unknown }).deltas;
            if (!Array.isArray(deltas) || latestSnapshot === null) return;
            for (const delta of deltas) {
              if (!delta || typeof delta !== "object") continue;
              const patch = (delta as { patch?: unknown }).patch;
              if (!patch || typeof patch !== "object") continue;
              const patchRecord = patch as Record<string, unknown>;
              const merged: Record<string, unknown> = { ...latestSnapshot };
              for (const key of ["config", "revision", "availability", "modelTransition"]) {
                if (key in patchRecord) merged[key] = patchRecord[key];
              }
              latestSnapshot = merged;
            }
          }
        },
      });
      // 钩子必须在 run() 之前接好：EventFire 与 subscribe ack 都可能先于轮询到达。
      sendChannel.onRpcFrame = (payload) => {
        currentLoop?.onData(payload);
        topicSubscription.onData(payload);
      };
      const subscribe = await topicSubscription.run();
      bridgeId = topicSubscription.lastRequestId;
      bridgeSeq = topicSubscription.lastSeq;
      bridgeMessageSeq = topicSubscription.lastMessageSeq;
      record(ctx, "send", "conversation-topic-subscribe", {
        topic: `conversation/${redactor.register("sid", bound.taskId)}`,
        outcome: subscribe.kind,
        ackMode: subscribe.kind === "ok" ? subscribe.ackMode : null,
        counters: { ...topicSubscription.counters },
        note:
          "EventListen(onDynamicConversationFrame,workspace) → subscribeConversationV4 → " +
          "ack；topic=conversation/<taskId>，ack 前帧暂存、ack 后按 subscriptionId 放行",
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

      // —— B3 交互半场（--interaction）：触发器 + 单帧应答/打断闭环 ——
      // 与 marker 发送互斥（两把锁独立）：本分支恰 1 个触发帧 + 至多 1 个
      // resolveInteraction/stop 帧；unknown 不重发（InteractionCommandLoop 结构性
      // 无重发路径）。所有应答只回传 pending 卡原值（optionId/题干/选项 value），
      // fullAccess 特权项在客户端硬闸排除。

      // —— B4 选择半场（--interaction=select）：读配置 → 切档 → 确认 → 恢复原值 ——
      // 走的仍是同一条已验证命令通道（sendConversationCommandV4 信封 +
      // switchModelConfig，command.ts:254-258；CAS baseRevision :315-331）。
      // 时长约束：switch 一个命令帧 + restore 一个命令帧（各自有界 ACK 窗，未知不重发）。
      if (interactionMode === "select") {
        const selection: Record<string, unknown> = { mode: "select", attempted: true };
        sendSummary.interaction = selection;
        const sleepMs = (ms: number): Promise<void> =>
          new Promise((resolve) => setTimeout(resolve, ms));
        const waitSnapshot = async <T>(probe: () => T | null, ms: number): Promise<T | null> => {
          const deadline = Date.now() + ms;
          for (;;) {
            const hit = probe();
            if (hit !== null) return hit;
            if (Date.now() >= deadline) return null;
            await sleepMs(25);
          }
        };
        const snapshot = await waitSnapshot(() => latestSnapshot, BRIDGE_RPC_TIMEOUT_MS + 5_000);
        if (snapshot === null) {
          sendVerdict = "UNAVAILABLE:no-snapshot";
        } else {
          const original = readConfigFromSnapshot(snapshot);
          const availability = (
            snapshot as { availability?: { switchModelConfig?: { allowed?: unknown } } }
          ).availability?.switchModelConfig;
          const revisionRaw = (snapshot as { revision?: unknown }).revision;
          let baseRevision = typeof revisionRaw === "number" ? revisionRaw : null;
          selection.original = original
            ? { provider: original.provider, model: original.model, thought: original.thought }
            : null;
          selection.availability = availability ?? null;
          if (original === null || baseRevision === null) {
            sendVerdict = "UNAVAILABLE:config-unreadable";
          } else if (availability && availability.allowed === false) {
            sendVerdict = "UNAVAILABLE:switch-disabled";
          } else {
            // 目录：先试 model-selection.getView()（mobileRemote 角色可见性未定），
            // 失败/空则退回会话快照目录（provider + 当前 model + config.thoughtLevels）。
            const viewOutcome = await sendChannel.bridgeRequest(
              conn,
              {
                id: bridgeId + 1,
                channel: "model-selection",
                method: "getView",
                identity,
                seq: bridgeSeq + 1,
                messageSeq: bridgeMessageSeq + 1,
              },
              BRIDGE_RPC_TIMEOUT_MS
            );
            bridgeId += 1;
            bridgeSeq += 1;
            bridgeMessageSeq += 1;
            const viewCatalog =
              viewOutcome.kind === "ok" ? resolveCatalogFromView(viewOutcome.decoded.body) : null;
            const viewBody =
              viewOutcome.kind === "ok"
                ? (viewOutcome.decoded.body as Record<string, unknown> | null)
                : null;
            selection.catalog = {
              source: viewCatalog ? "model-selection.getView" : "snapshot.config",
              outcome: viewOutcome.kind,
              shape: viewBody
                ? {
                    keys: Object.keys(viewBody).slice(0, 12),
                    providersType: Array.isArray(viewBody.providers)
                      ? "array"
                      : typeof viewBody.providers,
                    nested: ["data", "result"].filter(
                      (key) => viewBody[key] && typeof viewBody[key] === "object"
                    ),
                  }
                : null,
              providers:
                viewCatalog?.map((entry) => ({
                  provider: entry.provider,
                  models: entry.models,
                })) ?? null,
            };
            const catalogEntry =
              viewCatalog?.find((entry) => entry.provider === original.provider) ??
              viewCatalog?.find((entry) => entry.models.includes(original.model)) ??
              null;
            const altModel = catalogEntry?.models.find((model) => model !== original.model) ?? null;
            const targetModel = altModel ?? original.model;
            const targetThought =
              altModel === null
                ? (original.thoughtLevels.find((level) => level !== original.thought) ?? null)
                : original.thoughtLevels.includes(original.thought ?? "")
                  ? original.thought
                  : (original.thoughtLevels[0] ?? null);
            if (targetThought === null) {
              sendVerdict = "UNAVAILABLE:no-alternative";
            } else {
              const sendSwitch = async (
                model: string,
                thought: string,
                revision: number
              ): Promise<{ kind: string; reasonCode?: string | null }> => {
                const loop = new InteractionCommandLoop({
                  sender: conn,
                  identity,
                  target: bound,
                  commandType: "switchModelConfig",
                  payload: { provider: original.provider, model, thought },
                  baseRevision: revision,
                  clientId,
                  ackMs: SEND_ACK_TIMEOUT_MS,
                  channelRequestId: bridgeId + 1,
                  firstSeq: bridgeSeq + 1,
                  firstMessageSeq: bridgeMessageSeq + 1,
                });
                currentLoop = loop;
                const outcome = await loop.run();
                currentLoop = null;
                bridgeId += 1;
                bridgeSeq += 1;
                bridgeMessageSeq += 1;
                return outcome;
              };
              selection.target = {
                provider: original.provider,
                model: targetModel,
                thought: targetThought,
                modelChanged: targetModel !== original.model,
              };
              selection.switch = await sendSwitch(targetModel, targetThought, baseRevision);
              const confirmed = await waitSnapshot(() => {
                const current = readConfigFromSnapshot(latestSnapshot);
                return current && current.thought === targetThought && current.model === targetModel
                  ? current
                  : null;
              }, 15_000);
              selection.confirmed = confirmed
                ? { model: confirmed.model, thought: confirmed.thought }
                : null;
              // 恢复原值（best-effort；即使确认失败也执行）
              const revisionNow = (latestSnapshot as { revision?: unknown } | null)?.revision;
              let restored = false;
              if (original.thought) {
                const restoreRevision =
                  typeof revisionNow === "number" ? revisionNow : baseRevision;
                selection.restore = await sendSwitch(
                  original.model,
                  original.thought,
                  restoreRevision
                );
                const restoredConfig = await waitSnapshot(() => {
                  const current = readConfigFromSnapshot(latestSnapshot);
                  return current &&
                    current.thought === original.thought &&
                    current.model === original.model
                    ? current
                    : null;
                }, 15_000);
                restored = restoredConfig !== null;
              } else {
                selection.restore = { kind: "skipped", reasonCode: "no-original-thought" };
              }
              selection.restored = restored;
              if (confirmed === null) {
                sendVerdict =
                  selection.switch && (selection.switch as { kind: string }).kind === "accepted"
                    ? "UNKNOWN:config-not-observed"
                    : `REJECTED:${String((selection.switch as { reasonCode?: unknown })?.reasonCode ?? "switch-failed")}`;
              } else {
                sendVerdict = restored
                  ? "COMPLETED:selection-switch-restored"
                  : "COMPLETED:selection-switch-restore-unconfirmed";
              }
            }
          }
        }
        record(ctx, "interaction", "selection", selection);
        log(`selection verdict = ${sendVerdict}`);
      } else if (interactionMode !== null) {
        const interaction: Record<string, unknown> = {
          mode: interactionMode,
          attempted: true,
          markerPrefix: INTERACTION_MARKER_PREFIX,
          pendingWaitMs: INTERACTION_PENDING_WAIT_MS,
          foregroundWaitMs: INTERACTION_FOREGROUND_WAIT_MS,
        };
        sendSummary.interaction = interaction;
        const sleepMs = (ms: number): Promise<void> =>
          new Promise((resolve) => setTimeout(resolve, ms));
        const waitUntil = async <T>(probe: () => T | null, ms: number): Promise<T | null> => {
          const deadline = Date.now() + ms;
          for (;;) {
            const hit = probe();
            if (hit !== null) return hit;
            if (Date.now() >= deadline) return null;
            await sleepMs(25);
          }
        };

        // 1) 合成触发器（复用 B2 已验证发送链；恰一帧，未知不重发）
        const trigger = new SendOnceLoop({
          sender: conn,
          identity,
          target: bound,
          markerText: buildInteractionInstruction(interactionMode),
          clientId,
          timeouts: { ackMs: SEND_ACK_TIMEOUT_MS, activityMs: SEND_ACTIVITY_TIMEOUT_MS },
          channelRequestId: bridgeId + 1,
          firstSeq: bridgeSeq + 1,
          firstMessageSeq: bridgeMessageSeq + 1,
          isTurnActivity: (decoded, rawJson) =>
            decoded?.headerType === RESPONSE_TYPE_EVENT_FIRE && rawJson.includes(bound.taskId),
        });
        // 观察窗必须在 trigger.run() **之前**开始（C1）——trigger 的 ACK/活动窗可长达
        // 20s+30s，触发期间落下的 pending 卡（或新前台执行）不能等 trigger 返回后才开始找。
        // 关联（I1）：pending.createdAt 必须 >= 触发帧发送时刻；卡上缺 createdAt
        // （旧快照/合成夹具）时回退为「触发后出现的首张卡」（快照本身是触发后收到的）。
        const triggerSentAt = Date.now();
        const controlRevisionBaseline = book.controlRevision;
        // --existing-pending：用户**显式指定**应答会话中已存在的卡——不做「触发后」关联，
        // 且不触发新指令（仅 approve/reject/answer 适用；interrupt 仍需本 run 的前台执行）。
        const isFreshPending = (item: ObservedPending): boolean =>
          parsed.existingPending || item.createdAt === null || item.createdAt >= triggerSentAt;
        const isInterrupt = interactionMode === "interrupt";
        const pendingObservation: Promise<ObservedPending | null> = isInterrupt
          ? Promise.resolve(null)
          : interactionMode === "approve" || interactionMode === "reject"
            ? waitUntil(
                () =>
                  book.list().find((item) => item.kind === "permission" && isFreshPending(item)) ??
                  null,
                INTERACTION_PENDING_WAIT_MS
              )
            : waitUntil(
                () =>
                  book.list().find((item) => item.kind === "userInput" && isFreshPending(item)) ??
                  null,
                INTERACTION_PENDING_WAIT_MS
              );
        const stopObservation: Promise<{ kind: string; foregroundExecutionId: string } | null> =
          isInterrupt
            ? waitUntil(
                () => book.observeStopTarget({ afterRevision: controlRevisionBaseline }),
                INTERACTION_FOREGROUND_WAIT_MS
              )
            : Promise.resolve(null);
        if (parsed.existingPending) {
          interaction.trigger = {
            skipped: "existing-pending-designated",
            triggerSentAt,
            controlRevisionBaseline,
            note: "用户显式指定会话中已存在的 pending；不触发新指令（无触发帧）",
          };
          record(ctx, "interaction", "existing-pending-designated", interaction.trigger);
          log("--existing-pending：跳过触发，直接应答会话中已存在的卡。");
        } else {
          currentLoop = trigger;
          const triggerOutcome = await trigger.run();
          currentLoop = null;
          bridgeId += 1;
          bridgeSeq += 1;
          bridgeMessageSeq += 1;
          interaction.trigger = {
            outcome: triggerOutcome,
            framesSent: trigger.framesSent,
            peerAcknowledged: trigger.peerAcknowledged,
            triggerSentAt,
            controlRevisionBaseline,
          };
          record(ctx, "interaction", "trigger", interaction.trigger);
          log(
            `interaction trigger = ${triggerOutcome.kind}；framesSent=${trigger.framesSent}，peerAck=${trigger.peerAcknowledged}。`
          );
        }

        // 2) 宿主侧观察窗（并发启动、此处取结果；pending 卡 / 前台执行 id）；超窗如实记
        // unavailable。触发窗内落下的卡不会晚 20s 才被看见（C1 的另一半）。
        const observedPending = await pendingObservation;
        const observedStopTarget = await stopObservation;
        // 诊断（B3 实跑 024023Z 教训）：请求 kind 未命中时，把场上**实际存在的卡**
        // 的形状（kind/别名 id/payload 键名）如实落证——不猜、不消费、不发送。
        if (observedPending === null && !isInterrupt) {
          record(ctx, "interaction", "pending-inventory", {
            count: book.size,
            items: book.list().map((item) => {
              const payload = item.payload as Record<string, unknown>;
              const schema = payload.schema;
              const schemaInteraction =
                schema && typeof schema === "object"
                  ? ((schema as Record<string, unknown>).interaction ?? null)
                  : null;
              const questions = payload.questions;
              return {
                kind: item.kind,
                interactionId: redactor.register("sid", item.interactionId),
                toolName: typeof payload.toolName === "string" ? payload.toolName : null,
                schemaInteraction: typeof schemaInteraction === "string" ? schemaInteraction : null,
                questionCount: Array.isArray(questions) ? questions.length : 0,
                payloadKeys: Object.keys(payload).slice(0, 24),
                autoResolution: item.autoResolution,
                createdAt: item.createdAt,
              };
            }),
            bookCounters: book.counters,
          });
        }
        let commandType: InteractionCommandType | null = null;
        let payload: Record<string, unknown> | null = null;
        let unavailable: string | null = null;
        let autoResolution = false;
        let pendingInteractionId: string | null = null;

        // I6：题干 / 选项 label / 选项 value 先登记别名，再记录证据（卡上真实文本
        // 绝不原样进 transcript）。
        const registerQuestionMaterial = (pending: ObservedPending): Record<string, unknown> => {
          const questions = Array.isArray((pending.payload as { questions?: unknown }).questions)
            ? ((pending.payload as { questions: unknown[] }).questions as unknown[])
            : [];
          const aliases: Array<Record<string, unknown>> = [];
          for (const question of questions) {
            if (!question || typeof question !== "object") continue;
            const record = question as { question?: unknown; options?: unknown };
            const options = Array.isArray(record.options) ? record.options : [];
            aliases.push({
              question:
                typeof record.question === "string"
                  ? redactor.register("name", record.question)
                  : null,
              options: options.map((option) => {
                if (!option || typeof option !== "object") return null;
                const entry = option as { value?: unknown; label?: unknown };
                return {
                  value:
                    typeof entry.value === "string" ? redactor.register("name", entry.value) : null,
                  label:
                    typeof entry.label === "string" ? redactor.register("name", entry.label) : null,
                };
              }),
            });
          }
          return { questionAliases: aliases };
        };

        const recordPendingObserved = (pending: ObservedPending): void => {
          const options = (pending.payload as { options?: unknown }).options;
          const optionSummaries = Array.isArray(options)
            ? options
                .map((option) => {
                  if (!option || typeof option !== "object") return null;
                  const entry = option as { optionId?: unknown; kind?: unknown; label?: unknown };
                  return {
                    optionId:
                      typeof entry.optionId === "string"
                        ? entry.optionId === PERMISSION_FULL_ACCESS_OPTION_ID
                          ? "option:fullAccess(blocked)"
                          : redactor.register("name", entry.optionId)
                        : null,
                    kind: typeof entry.kind === "string" ? entry.kind : null,
                    label:
                      typeof entry.label === "string"
                        ? redactor.register("name", entry.label)
                        : null,
                  };
                })
                .filter((entry) => entry !== null)
            : [];
          record(ctx, "interaction", "pending-observed", {
            kind: pending.kind,
            interactionId: redactor.register("sid", pending.interactionId),
            createdAt: pending.createdAt,
            triggerSentAt,
            autoResolution: pending.autoResolution,
            autoResolutionState: pending.autoResolutionState,
            optionIds: collectOptionIdsFromOptions(options).map((id) =>
              redactor.register("name", id)
            ),
            optionSummaries,
            fullAccessOptionPresent: pending.payload.fullAccessOption !== undefined,
            ...(pending.kind === "userInput" ? registerQuestionMaterial(pending) : {}),
            bookCounters: { ...book.counters },
          });
        };

        if (interactionMode === "approve" || interactionMode === "reject") {
          if (!observedPending) {
            unavailable = "UNAVAILABLE:no-pending-observed";
          } else {
            const pending = observedPending;
            autoResolution = pending.autoResolution;
            pendingInteractionId = pending.interactionId;
            recordPendingObserved(pending);
            const optionId = pickPermissionOption(pending, interactionMode);
            if (optionId === null) {
              unavailable = "UNAVAILABLE:permission-option-unresolved";
            } else {
              const gated = authorizePendingResolution(book, pending.interactionId, {
                sessionId: bound.taskId,
                liveGeneration: CONNECTION_GENERATION,
              });
              commandType = INTERACTION_COMMAND_TYPES.resolve;
              payload = {
                interactionId: gated.interactionId,
                answer: buildPermissionAnswer({ optionId }),
              };
            }
          }
        } else if (interactionMode === "answer") {
          if (!observedPending) {
            unavailable = "UNAVAILABLE:no-pending-observed";
          } else if (!isAnswerableUserInput(observedPending)) {
            // C2：计划审批（ExitPlanMode）/ MCP elicitation / 未知工具都投影成
            // userInput，只有 AskUserQuestion 可答；其余一律不支持，零帧。
            record(ctx, "interaction", "answer-shape-unsupported", {
              kind: observedPending.kind,
              toolName:
                typeof observedPending.payload.toolName === "string"
                  ? redactor.register("name", observedPending.payload.toolName)
                  : null,
              schemaInteraction:
                observedPending.payload.schema &&
                typeof observedPending.payload.schema === "object" &&
                typeof (observedPending.payload.schema as { interaction?: unknown }).interaction ===
                  "string"
                  ? (observedPending.payload.schema as { interaction: string }).interaction
                  : null,
              note:
                "only AskUserQuestion userInput is answerable " +
                "(product-projection.ts:3391-3409,5793-5795; plan approval :3440-3459)",
            });
            unavailable = "UNAVAILABLE:answer-shape-unsupported";
            {
              const payload = observedPending.payload as Record<string, unknown>;
              const schema = payload.schema;
              const schemaInteraction =
                schema && typeof schema === "object"
                  ? ((schema as Record<string, unknown>).interaction ?? null)
                  : null;
              log(
                `answer gate 未通过：toolName=${String(payload.toolName ?? "")} schema.interaction=${String(
                  schemaInteraction ?? ""
                )}`
              );
            }
          } else {
            const pending = observedPending;
            autoResolution = pending.autoResolution;
            pendingInteractionId = pending.interactionId;
            recordPendingObserved(pending);
            const entries = pickUserInputEntries(pending);
            if (entries === null) {
              unavailable = "UNAVAILABLE:answer-shape-unresolved";
            } else {
              const gated = authorizePendingResolution(book, pending.interactionId, {
                sessionId: bound.taskId,
                liveGeneration: CONNECTION_GENERATION,
              });
              commandType = INTERACTION_COMMAND_TYPES.resolve;
              payload = {
                interactionId: gated.interactionId,
                answer: buildUserInputAnswer(entries),
              };
            }
          }
        } else if (!observedStopTarget) {
          // 触发后没有新的 primaryTurn 前台执行 id → 不做盲停（Minor：UNAVAILABLE 而非 NOOP）。
          unavailable = "UNAVAILABLE:no-foreground-execution";
        } else {
          commandType = INTERACTION_COMMAND_TYPES.stop;
          payload = buildStopPayload(observedStopTarget);
          interaction.stopTarget = {
            kind: observedStopTarget.kind,
            foregroundExecutionId: redactor.register(
              "sid",
              observedStopTarget.foregroundExecutionId
            ),
            observedAfterControlRevision: controlRevisionBaseline,
          };
        }

        interaction.channel = {
          ...book.counters,
          pendingSize: book.size,
          // 订阅链证据：EventFire / wire 帧 / fragment 重组 / 交付计数（不含 subscriptionId）。
          subscription: { ...topicSubscription.counters },
        };
        if (unavailable !== null) {
          sendVerdict = unavailable;
          interaction.resolution = { attempted: false, reason: unavailable };
          record(ctx, "interaction", "unavailable", interaction.resolution);
          log(`interaction 不可用：${unavailable}（未发送任何应答帧）。`);
        } else if (commandType !== null && payload !== null) {
          // I4：卡上带 autoResolution（AskUserQuestion 自动结束计时）时，先发
          // snoozeInteractionAutoResolution 再发 resolve（首次有效操作永久暂停本次
          // 自动结束；重复/迟到调用幂等 noop——command.ts:250-253）。snooze 只是
          // 尽力而为的前置帧：失败/超窗如实记录后继续 resolve（resolve 本身先到先得，
          // 若自动结束已抢先收口则 resolve 回 proto.alreadyResolved noop，绝不重发）。
          const steps = planInteractionCommands({
            pending: {
              interactionId: pendingInteractionId ?? "",
              autoResolution,
            },
            commandType,
            payload,
          });
          for (const step of steps) {
            const loop = new InteractionCommandLoop({
              sender: conn,
              identity,
              target: bound,
              commandType: step.commandType,
              payload: step.payload,
              clientId,
              ackMs: SEND_ACK_TIMEOUT_MS,
              channelRequestId: bridgeId + 1,
              firstSeq: bridgeSeq + 1,
              firstMessageSeq: bridgeMessageSeq + 1,
            });
            currentLoop = loop;
            const outcome = await loop.run();
            currentLoop = null;
            bridgeId += 1;
            bridgeSeq += 1;
            bridgeMessageSeq += 1;
            if (step.commandType === INTERACTION_COMMAND_TYPES.snooze) {
              interaction.snooze = {
                attempted: true,
                outcome,
                framesSent: loop.framesSent,
                peerAcknowledged: loop.peerAcknowledged,
                autoResolutionState: observedPending?.autoResolutionState ?? null,
                note: "best-effort: failure is recorded and resolve still proceeds (first-wins)",
              };
              record(ctx, "interaction", "snooze", interaction.snooze);
              continue;
            }
            interaction.resolution = {
              attempted: true,
              outcome,
              framesSent: loop.framesSent,
              peerAcknowledged: loop.peerAcknowledged,
              duplicateInboundFrames: loop.duplicateInboundFrames,
              autoResolution,
            };
            record(ctx, "interaction", "resolution", interaction.resolution);
            sendVerdict =
              outcome.kind === "accepted"
                ? interactionMode === "interrupt"
                  ? "STOPPED:interrupt-accepted"
                  : "RESOLVED"
                : outcome.kind === "noop"
                  ? `NOOP:${outcome.reasonCode ?? "unknown"}`
                  : outcome.kind === "rejected"
                    ? `REJECTED:${outcome.reasonCode ?? "unknown"}`
                    : outcome.kind === "send-failed"
                      ? "STOPPED:interaction-send-failed"
                      : "UNKNOWN";
            log(
              `interaction outcome = ${outcome.kind}；framesSent=${loop.framesSent}，peerAck=${loop.peerAcknowledged}。`
            );
          }
        }
      }

      if (interactionMode === null) {
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
        // 复用同一单槽帧汇：topic 订阅器继续消费 conversation 帧（活动判据见 loop）。
        currentLoop = loop;
        const sendOutcome = await loop.run();
        currentLoop = null;
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
            sendOutcome.kind === "completed"
              ? `（activityFrames=${sendOutcome.activityFrames}）`
              : ""
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
      }
      // 事件流订阅证据（两种 send 模式都记）：EventFire / wire / fragment / 交付计数
      // （不含 subscriptionId 真实值）。
      sendSummary.subscription = { ...topicSubscription.counters };
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
    // stdout 同样只出别名（与证据同口径）：异常 message 可能内嵌真实路径/会话 id。
    log(
      `探针故障（${stage} 阶段未处理异常）：${redactor.sanitizeText(message)}；清理连接并照常落证。`
    );
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
 * app payload 应答摘要（证据只落 sanitized 形态）。success 三态：
 * true / false / null（缺席）——workspace-bridge-ready **不携带 success 字段**
 * （web-remote-control.ts:449-456；manager.ts:1156-1165），旧实现
 * `success: payload.success === true` 曾把 ready 误报成 success:false
 * （B2 实连 macos-B220261024T124743Z 的假「failure」）。
 */
export function summarizePayload(outcome: AppPayloadOutcome): Record<string, unknown> {
  if (outcome.kind !== "ok") return { outcome: outcome.kind };
  const payload = outcome.payload;
  return {
    outcome: "ok",
    zcode_type: typeof payload.zcode_type === "string" ? payload.zcode_type : null,
    success: payload.success === true ? true : payload.success === false ? false : null,
    error: typeof payload.error === "string" ? payload.error : null,
    reason: typeof payload.reason === "string" ? payload.reason : null,
    payload: payload,
  };
}

/** 收帧证据落 events（有界 + 截断 + 脱敏；真实值经 sanitizeText 只余别名）。 */
function recordChannelEvidence(ctx: RunContext, label: string, channel: B2Channel): void {
  const messages = channel.bridgeMessages.slice(0, 64).map((message) => {
    // I6：结构化预览先过 redactPayload（字段级别名登记，workspacePath/taskId/... 等），
    // 再统一走 sanitizeText；非 JSON 消息退回纯文本脱敏。
    const preview = (() => {
      try {
        return (
          JSON.stringify(redactPayload(ctx.redactor, JSON.parse(message) as unknown)) ?? message
        );
      } catch {
        return message;
      }
    })();
    const sanitized = ctx.redactor.sanitizeText(preview);
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
    acksSent: channel.transportAcksSent,
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
  const isInteraction = ctx.flags.interaction !== null;
  // I5：证据里的安全闸按模式派生（交互 run 实际 2 个命令帧 + 可达 1 个 snooze 前置帧，
  // 不再把 B2 的 maxSendFrames:1 抄进 B3 证据）。
  const guards = deriveProbeGuards(ctx.flags.interaction);
  const transcript = {
    runId: ctx.run.runId,
    probe: isInteraction ? "zcode-bridge-interaction-probe" : "zcode-bridge-session-probe",
    workPackage: isInteraction
      ? "B3 交互半场：审批 / 问答 / 打断逐能力探针（单连接、映射与 target lock 复用 B2 链）"
      : "B2 实连半场：单连接、会话映射与单次发送闭环",
    sourceBaseline: "zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab",
    startedUtc: ctx.startedIso,
    finishedUtc: finishedIso,
    guards,
    flags: {
      provider: ctx.flags.provider,
      unlock: ctx.flags.unlock,
      sendAuthorized: ctx.flags.sendAuthorized,
      interaction: ctx.flags.interaction,
      interactionAuthorized: ctx.flags.interactionAuthorized,
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
    `# ${isInteraction ? "B3 交互半场" : "B2 实连半场"}探针 run — ${ctx.run.runId}`,
    "",
    `- started: ${ctx.startedIso}`,
    `- finished: ${finishedIso}`,
    `- oauth: **${oauthVerdict}**`,
    `- connect: **${connectVerdict}**`,
    `- mapping: **${mappingVerdict}**`,
    `- ${isInteraction ? "interaction" : "send"}: **${sendVerdict}**`,
    `- guards: \`${JSON.stringify(guards)}\``,
    "",
    "本目录为私有证据（不进仓库）。所有 SID/JWT/token/state/授权码/链接/会话 id",
    "均以稳定别名与掩码形态记录；落盘前经 fail-closed 断言。marker 正文为合成值",
    isInteraction
      ? "（[tuvis-b3-probe:<UTC>]，不含任何秘密）。指令帧恒 1、应答/stop 帧恒 1（autoResolution 在场时至多加 1 个 snooze 前置帧）；未知不重发。"
      : "（[tuvis-b2-marker:<UTC>]，不含任何秘密）。发送帧恒 1；未知不重发。",
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

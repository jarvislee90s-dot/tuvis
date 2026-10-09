// 移动端 API：纯 fetch，零 Tauri 依赖（PWA/浏览器同构）
import type { SessionsResponse, TransitionEvent } from "@/types/session";

/** 带 HTTP 状态的请求失败（status=null 表示网络层异常，无响应可读）。
 *  详情页/预览按 status 分流错误文案（404 → 会话不可读，403 → 文件不可预览）；
 *  data 携带错误响应体 JSON（/pair/pin 的 error/remaining/retryAfter），供 PairPage 分診文案 */
export class ApiError extends Error {
  status: number | null;
  data: Record<string, unknown> | null;
  constructor(status: number | null, message: string, data: Record<string, unknown> | null = null) {
    super(message);
    this.status = status;
    this.data = data;
  }
}

/** 访问密码配对（M5 A7，唯一配对入口）：POST /pair/pin。
 *  成功 → { ok: true }（180 天 cookie 已由响应 Set-Cookie 落地）；
 *  失败 → 抛 ApiError：HTTP 状态在 status、响应体 JSON（error/remaining/retryAfter）
 *  在 data——PairPage 据此分診「剩余次数 / 锁定 / 未设密码 / 设备满 / 网络」文案。
 *  429（限速锁定）与 403（设备上限）也走 ApiError（不再像旧 /pair 静默吞掉） */
export async function pairWithPin(pin: string): Promise<{ ok: boolean }> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/pair/pin", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ pin }),
    });
  } catch (e) {
    throw new ApiError(null, `pair/pin 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 错误体非 JSON（代理注入页等）：data 保持 null，上层按状态码兜底文案 */
    }
    throw new ApiError(r.status, `pair/pin ${r.status}`, data);
  }
  return { ok: true };
}

export async function fetchSessions<T>(): Promise<T | null> {
  const r = await fetch("/m/api/v1/sessions");
  if (r.status === 403) return null; // 设备失效 → 回配对页
  return r.json() as Promise<T>;
}

// host 载荷（GET /m/api/v1/host）：host 部分对应 Rust host_payload 的 "host" 键；
// enabledTools 为 P8d 受管工具 id 列表（Task 3 chips 过滤的数据源）；
// bootId 为 兔维斯 进程生命周期标识（书签等「随进程消失」的客户端态的恢复守卫）
export interface HostInfo {
  name: string;
  platform: "macos" | "windows" | "linux";
  version: string;
  bootId: string;
}

export interface HostPayload {
  host: HostInfo;
  enabledTools: string[];
  /** 安装探测结果（P1-9，2026-10-03）：新建会话四家中 PATH 探测命中的子集——
   * 前端「未安装」置灰的数据源（与 enabledTools 分列：受管 ≠ 已安装） */
  installedTools: string[];
}

export async function fetchHost<T = HostPayload>(): Promise<T | null> {
  const r = await fetch("/m/api/v1/host");
  if (r.status === 403) return null; // 设备失效 → 与 sessions 同语义
  return r.json() as Promise<T>;
}

// ==== Task 10 §C5：通道能力装饰（带宽如实告知）====

/** 通道能力载荷（GET /m/api/v1/channel，与 Rust `channel_info` 逐字段对应）：
 *  via ∈ quick/named/tailscale/lan（Host 推断，可被伪造）；limited = 隧道受限；
 *  estMbpsDown/Up = 服务端实测速率（下行客户端实收 / 上行服务端实收口径）。
 *  **纯装饰**：允许不准，永不得进任何安全判定——只喂文件面板的带宽提示文案 */
export interface ChannelInfo {
  via: string;
  limited: boolean;
  estMbpsDown: number;
  estMbpsUp: number;
}

/** 仅接受有限正数；其余（NaN / Infinity / 字符串非数 / 缺失）一律 0，由调用方以 `>0` 判可用。 */
function finitePositive(v: unknown): number {
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? n : 0;
}

/** 拉取通道能力（FilePanel 挂载时一次）。任何失败（403 设备失效 / 网络 / 载荷
 *  异常）→ null：装饰能力静默降级为「无提示」，不阻塞面板（fetchHost 同口径） */
export async function fetchChannel(): Promise<ChannelInfo | null> {
  try {
    const r = await fetch("/m/api/v1/channel");
    if (!r.ok) return null;
    const j = (await r.json()) as {
      via?: unknown;
      limited?: unknown;
      est_mbps_down?: unknown;
      est_mbps_up?: unknown;
    };
    if (typeof j.via !== "string" || typeof j.limited !== "boolean") return null;
    return {
      via: j.via,
      limited: j.limited,
      // 纵深防御：除 NaN/缺失外还要挡 Infinity（JSON 字面 `1e999` 会被解析为 Infinity，
      // 而 `Infinity > 0` 为真 → 会算出"0.0 分钟"这种看似合理的假耗时）
      estMbpsDown: finitePositive(j.est_mbps_down),
      estMbpsUp: finitePositive(j.est_mbps_up),
    };
  } catch {
    return null;
  }
}

import type { UiConfig } from "./theme";

// ==== 2026-10-05 UI 改版：远程端外观配置（spec §6.2）====
// 载荷结构 UiConfig 定义在 theme.ts（配置应用与镜像同域），此处仅消费

/** 拉取远程端外观配置。403（设备失效）→ null（与 fetchHost 同口径）；
 *  其余非 2xx / 网络异常 → 抛 ApiError，调用方静默保留现有属性（外观是增强
 *  能力，任何失败都不阻塞看板——spec §6.2 回退口径） */
export async function fetchUiConfig(): Promise<UiConfig | null> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/ui-config");
  } catch (e) {
    throw new ApiError(null, `ui-config 网络异常: ${String(e)}`);
  }
  if (r.status === 403) return null;
  if (!r.ok) throw new ApiError(r.status, `ui-config ${r.status}`);
  const j = (await r.json()) as Partial<UiConfig>;
  return {
    daySkin: String(j.daySkin ?? "lpaper"),
    nightSkin: String(j.nightSkin ?? "npaper"),
    font: String(j.font ?? "std"),
    radius: typeof j.radius === "number" ? j.radius : 12,
    accent: String(j.accent ?? "edge"),
  };
}

// ==== M3 Task 8：会话详情（ZCode 式对话视图）+ 文件预览 ====

/** 统一消息条目 — 与 Rust `remote::content::SessionMessage`（camelCase 序列化）逐字段
 *  对应，勿漂移：seq / role / content / kind / ts / toolName? / toolArgs? / collapsed。
 *  kind ∈ user / assistant / thinking / tool-call / tool-result / plan；
 *  thinking 与 tool-call 的 collapsed 恒 true（wire 语义，运行中态的默认折叠依据）；
 *  plan 是 T1 升格的一等计划消息（content = 计划 markdown 原文，collapsed 恒 false，
 *  toolName 保留供辨识、toolArgs 恒空——不透传参数串） */
export interface SessionMessage {
  seq: number;
  role: string;
  content: string;
  kind: "user" | "assistant" | "thinking" | "tool-call" | "tool-result" | "plan" | string;
  ts: number | null;
  toolName?: string | null;
  toolArgs?: string | null;
  collapsed: boolean;
}

/** 会话消息页（Bug 1，M3 验收）：messages + truncated（文件头部被字节窗截断的
 *  标记——SQLite 系与 dsh 恒 false）。truncated=true 表示存在更早未在本页的内容，
 *  即使条数 < limit 也应显示「加载更早消息」 */
export interface SessionMessagesPage {
  messages: SessionMessage[];
  truncated: boolean;
}

/** 拉取单会话消息流尾部（八工具统一出口）。读取失败（会话不存在 / 存储不可读）
 *  以 ApiError 抛出：404 = 会话内容不可读；网络异常 status=null。
 *  本层无状态：SSE transition 不驱动详情页（M3 范围裁决不变），10s 轮询节奏由
 *  SessionDetail 页面层驱动（F6），此处只负责单次拉取 */
export async function fetchSessionMessages(
  agentType: string,
  sessionId: string,
  limit: number
): Promise<SessionMessagesPage> {
  const q = new URLSearchParams({
    agent_type: agentType,
    session_id: sessionId,
    limit: String(limit),
  });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-messages?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-messages 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-messages ${r.status}`);
  const j = (await r.json()) as { messages?: SessionMessage[]; truncated?: boolean };
  return {
    messages: Array.isArray(j.messages) ? j.messages : [],
    truncated: j.truncated === true,
  };
}

/** 文件条目（M3+ 文件面板，与 Rust `remote::files::FileEntry` camelCase 序列化
 *  逐字段对应，勿漂移）：path = 最后一次出现的原始形态，lastSeq = 最后出现条目的
 *  会话内序，lastTs = 最后出现时间（可 null → 前端显示 `—`），hits = 出现次数
 *  （后端契约字段；2026-09-16 用户裁决：次数信息用户不在意，前端**不再展示**，
 *  保留字段供后续可能的排序/统计消费），
 *  modified = 是否被写类工具（Edit/Write…）改写触达（2026-09-16 用户裁决：
 *  前端据此把「仅读过」的文件名渲成常规色，与「动过手」的区分），
 *  origin = 来源三池（M5 B2）：user=我上传的 / tool_read=工具读取 / tool_write=
 *  工具读写；旧载荷无此键 → undefined（来源筛选按「全部来源」处理，行上不渲染徽标） */
export interface SessionFileEntry {
  path: string;
  lastSeq: number;
  lastTs: number | null;
  hits: number;
  modified: boolean;
  origin?: "user" | "tool_read" | "tool_write";
}

/** 拉取该会话涉及的文件表（/session-files，泛化提取）。一份数据两用：正文
 *  路径链接化（取 path 集）+ 文件面板列表（全字段）。增强能力：任何失败
 *  （含 404/403）静默降级为空表——详情页正文照常渲染。
 *  limit = 追溯档位（面板 200/500/1000 三档），透传后端窗口机制；
 *  truncated = 该档位下还有更早文件未纳入（面板据此提示） */
export async function fetchSessionFiles(
  agentType: string,
  sessionId: string,
  limit: number
): Promise<{ files: SessionFileEntry[]; truncated: boolean }> {
  const q = new URLSearchParams({
    agent_type: agentType,
    session_id: sessionId,
    limit: String(limit),
  });
  try {
    const r = await fetch(`/m/api/v1/session-files?${q}`);
    if (!r.ok) return { files: [], truncated: false };
    const j = (await r.json()) as { files?: SessionFileEntry[]; truncated?: boolean };
    return {
      files: Array.isArray(j.files) ? j.files : [],
      truncated: j.truncated === true,
    };
  } catch {
    return { files: [], truncated: false };
  }
}

/** 文件预览载荷：图片 → 同源 fetch blob 后的 object URL（调用方负责 revoke）；
 *  其余 → 文本内容 + 后端判定的 mime */
export type FilePayload =
  { kind: "image"; url: string; mime: string } | { kind: "text"; content: string; mime: string };

/** 安全读取会话项目目录内的文件（/file）。越界 / 超限 / 不存在对外一律 403
 *  （后端探测面最小化，不可区分）；session_id 不在快照 → 404；网络异常 → status=null。
 *  onProgress（Task 10 §C5 新增第 3 参）：图片走分块流式读，按累计字节回调
 *  （loaded, total）；total 取 content-length——axum/hyper 会隐式写入该头，
 *  但隧道（Cloudflare Funnel / Tailscale Funnel）是否原样透传**未经真机确认**
 *  （2026-10-07 登记：无真机隧道环境，唯一外部变量，待做）——拿不到就传 null，
 *  UI 退化为只显示已传字节、不显示假百分比。文本分支走 json()，无进度回调 */
export async function fetchFile(
  sessionId: string,
  filePath: string,
  onProgress?: (loaded: number, total: number | null) => void
): Promise<FilePayload> {
  const q = new URLSearchParams({ session_id: sessionId, path: filePath });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/file?${q}`);
  } catch (e) {
    throw new ApiError(null, `file 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    // M5 P2-a：403 响应体带结构化原因码（sensitive/too_large/not_found/not_file/io），
    // FilePreview 据此分診排障文案
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体（代理页等）：data 保持 null，按状态码兜底文案 */
    }
    throw new ApiError(r.status, `file ${r.status}`, data);
  }
  const mime = r.headers.get("content-type")?.split(";")[0]?.trim() ?? "";
  if (mime.startsWith("image/")) {
    // 下行进度：分块读 + 累计字节。total 取 content-length（axum/hyper 隐式写入，
    // 隧道是否透传待真机确认——拿不到就传 null，不编百分比，见函数注释）
    const total = Number(r.headers.get("content-length") ?? "") || null;
    let loaded = 0;
    const chunks: Uint8Array[] = [];
    if (r.body && onProgress) {
      const reader = r.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value);
        loaded += value.byteLength;
        onProgress(loaded, total);
      }
    }
    const blob = chunks.length
      ? new Blob(chunks as BlobPart[], { type: r.headers.get("content-type") ?? "" })
      : await r.blob();
    return { kind: "image", url: URL.createObjectURL(blob), mime };
  }
  const j = (await r.json()) as { content: string; mime: string };
  return { kind: "text", content: j.content, mime: j.mime };
}

// ==== M3 Task 6：SSE 实时通道客户端 ====

/** SSE 端点路径（唯一来源：EventSource 与测试 mock 都引用它） */
export const EVENTS_PATH = "/m/api/v1/events";

/** 断流后重连的退避基数（毫秒）：第 N 次失败等待 N×基准，N 达降级阈值即转轮询 */
export const RECONNECT_BASE_MS = 1000;

/** 降级阈值：连续失败达到该次数即放弃 SSE、转轮询
 *  （C1 验收「断 SSE → 2 次失败 → 3s 轮询」） */
export const DEGRADE_AFTER_FAILURES = 2;

/**
 * 订阅服务端事件流（GET /m/api/v1/events），返回停止函数。
 *
 * 两条数据通道：
 * - `onSnapshot(SessionsResponse)`：连接建立后的首帧全量快照（也是断线重连后的基线校正）；
 * - `onTransition(TransitionEvent)`：watcher 已去重的状态跃迁边沿（**铁律 4：本层不独立
 *   去重**，来一条转一条）。
 *
 * `onDegraded()`：连续失败达阈值后的**一次性**信号——SSE 通道放弃，由调用方切换轮询
 * （本函数**不接管轮询**：降级后的 3s 轮询复用调用方既有的 tick 逻辑，避免两套轮询
 * 并存；简报骨架里 connectEvents 自带轮询循环，与「复用现有 tick」的控制者裁决冲突，
 * 裁决优先——轮询留在 Board，in-flight 守卫 / 403→onUnpaired / 错误横幅语义单点保留）。
 *
 * 断线策略（控制者裁决，勿改成"依赖浏览器内建重连"）：`onerror` 里显式 `close()` +
 * 手动线性退避重连（N×1s），而非放任 EventSource 自愈——因为需要**可数的失败语义**
 * 才能实现「2 次失败降级轮询」；浏览器内建重连（约 3s 固定间隔）不可观测、与手动重连
 * 竞争，故关闭内建、自管节奏。
 *
 * 403 语义（与 fetchSessions 口径一致）：EventSource 拿不到 HTTP 状态码，403（设备失效）
 * 与网络断流对 `onerror` 不可区分——**这里一律不判设备失效**（SSE 断线不等于 403，
 * 服务器重启/网络抖动都会断流，误踢回配对页会打断用户）。设备失效判定唯一走
 * fetchSessions 返回 null 的通道（初始探测与降级轮询）。
 *
 * 不做「轮询期间定期试回 SSE」（YAGNI，M3 不要求）：服务端恢复后刷新页面即重连；
 * 常驻双通道探测会引入额外连接与状态机复杂度，收益不成比例。
 */
export function connectEvents(
  onSnapshot: (data: SessionsResponse) => void,
  onTransition: (data: TransitionEvent) => void,
  onDegraded: () => void
): () => void {
  let es: EventSource | null = null;
  let failures = 0;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let degraded = false;
  let stopped = false; // 停止后禁止任何重连再被装上

  function degrade() {
    if (degraded) return;
    degraded = true;
    es?.close();
    es = null;
    onDegraded();
  }

  /** 解析帧并转交回调；坏帧（截断 / 代理注入）跳过该帧，连接保持——
   *  不得因单帧 JSON 错误断流 */
  function dispatch<T>(e: Event, handler: (data: T) => void) {
    try {
      const parsed = JSON.parse((e as MessageEvent).data) as T;
      // 成功解析出一帧即清零：退避计数只反映"连续失败"（收到合法帧 = 链路健康）
      failures = 0;
      handler(parsed);
    } catch {
      /* 坏帧：忽略（不清零计数——能收到字节但解析不了不足以证明链路健康） */
    }
  }

  function connect() {
    if (stopped || degraded) return;
    let next: EventSource;
    try {
      next = new EventSource(EVENTS_PATH);
    } catch {
      // 环境不支持 EventSource（老浏览器 / 无该全局的运行时）：不做无谓退避重试，
      // 直接降级轮询——与「连不上即降级」同一终局，省掉注定失败的两次握手
      degrade();
      return;
    }
    es = next;
    next.addEventListener("snapshot", (e) => dispatch(e, onSnapshot));
    next.addEventListener("transition", (e) => dispatch(e, onTransition));
    next.onerror = () => {
      if (stopped || degraded) return;
      next.close(); // 关掉内建重连（见函数注释的断线策略）
      if (es === next) es = null;
      failures += 1;
      if (failures >= DEGRADE_AFTER_FAILURES) {
        degrade();
      } else {
        reconnectTimer = setTimeout(connect, RECONNECT_BASE_MS * failures); // 线性退避
      }
    };
  }

  connect();
  return () => {
    stopped = true;
    es?.close();
    es = null;
    if (reconnectTimer) clearTimeout(reconnectTimer);
  };
}

// ==== M7 Task 7：注入发送（W4 移动端发送 UI）====

/** 输入区可用性矩阵（GET /session-send-info 载荷，与 Rust `session_send_info`
 *  的 JSON 逐字段对应，勿漂移）：injectable=false 时 reasonCode/reason 携带不可
 *  注入原因（如 `headless_disabled` 总开关关闭 / `dsh_headless_pending` 写通道未接线 /
 *  路由层 `no_process` 等），**channels/visibility 不返回**（后端
 *  RouteOutcome::NotInjectable 分支只给 {injectable,reasonCode,reason}）→ 前端
 *  类型须 optional（M9R P2-10 对齐）；injectable=true 时 channels 为候选注入
 *  通道（tmux/iterm2/… 或 `headless_*`），visibility=after_refresh 表示注入后需刷新才见回显。
 *
 *  **注意（Task 11 起）**：`injectable:true` 表达的是**静态可注入能力**——WorkBuddy ACP
 *  这类无头通道的运行时不可用（远程控制端点未启用）**不在本载荷里预判**，由**发送回执**
 *  如实上报（`refused` + reason 文案）；故「输入区可用」≠「必达」，回执才是真相。 */
export interface SendInfo {
  injectable: boolean;
  reasonCode?: string;
  reason?: string;
  channels?: string[];
  /** 可见性档（后端 `routing::Visibility` 的 wire 词）。Task 8 补齐无头两档：
   *  `after_restart` = 已信任工作区（重启 ZCode 应用后可见）/ `tuvis_only` = 未信任（仅兔维斯
   *  可见）——两档文案由后端 `Visibility::note()` 下发，前端只渲染不编。 */
  visibility?: "realtime" | "after_refresh" | "after_restart" | "tuvis_only";
}

/** 无头通道名判定（send-info 的 channels 里是否含无头通道 `headless_*`）。
 *  **单一判据**：发送路径据此分流（无头回合 = 请求等整个进程跑完 + 可取消 + 回执卡），
 *  通道名与后端 `HeadlessKind::wire_name` 同源（词表只此一份）。 */
export function headlessChannelOf(info: SendInfo | null): string | null {
  return info?.channels?.find((c) => c.startsWith("headless_")) ?? null;
}

/** 无头回合回执（spec H6 形状，与 Rust `inject::headless::receipt::Receipt` 逐字段对应，
 *  勿漂移）：status ∈ ok|queued|failed|cancelled；stage 是**分阶段失败档**（zcode 专档
 *  `workspace_busy` = 应用争用锁）；可选键缺席即不上线（后端 `skip_serializing_if`）。
 *  `stage` 与 `refused`（投递前拒绝）的完整名单见 `tests/fixtures/headless_stages.json`
 *  ——前端分诊表与 Rust 枚举各自对照它断言（跨语言锁）。 */
export interface HeadlessReceipt {
  status: "ok" | "queued" | "failed" | "cancelled";
  sessionId: string;
  lastAssistant?: string;
  tokens?: number;
  durationMs: number;
  stage?:
    | "spawn"
    | "version_gate"
    | "timeout"
    | "crash"
    | "channel_error"
    | "dialog"
    | "workspace_busy"
    /** 投递前拒绝（回合未起跑、零字节投递：斜杠命令/会话串行锁/平台不支持） */
    | "refused";
  reason?: string;
}

/** 无头回合卡片态（SessionDetail 持有；MessageComposer 经 `onHeadlessTurn` 上报）：
 *  - `sending`：HTTP 在飞（无头回合 = 进程生命周期，实测 8–23s；期间只此一态 + 取消钮。
 *    `channel` = 本条走的无头通道 wire 名——**审批卡轮询只在 claude 通道上开**
 *    （其余通道没有审批面：codex queue/zcode yolo 无、kimi/opencode 由 CLI 自行拒绝））
 *  - `done`：终态回执（`receipt.status` 分诊：ok/queued 回执卡；failed 失败分诊卡；
 *    cancelled 取消卡——**三态都由 receipt 自身说话，前端不另编成功/失败**） */
export type HeadlessTurn =
  | { phase: "sending"; channel?: string | null }
  | {
      phase: "done";
      receipt: HeadlessReceipt;
      /** 可见性提示（后端 `Visibility::note()` 逐字文案；失败/取消时后端不给） */
      visibilityNote?: string | null;
    };

// ==== H5（Task 10 立接口）+ H11（Task 13/C4 激活）：无头审批 / 问答卡 ====

/** 决策词表（**跨语言夹具 `tests/fixtures/headless_decision_words.json` 的 `decisions`**；
 *  Rust 侧 `inject::headless::cli_three::Decision::wire` 与本常量各自对照同一夹具断言）。
 *  - `allow`：批准（后端把请求原始 input 原样回显成 `updatedInput`——前端**不回带 input**）；
 *  - `deny`：拒绝；**用户弃卡（关闭卡片）也必须发 deny**（附录 E-②：allow 但未答 = 静默丢题）；
 *  - `answer`：问答卡提交（**必须答全**，见 `HeadlessApprovalQuestion`）。 */
export const HEADLESS_DECISION_WORDS = ["allow", "deny", "answer"] as const;
export type HeadlessDecisionWord = (typeof HEADLESS_DECISION_WORDS)[number];

/** 待答种类词表（同一夹具的 `kinds`）：`approval` = 工具审批卡；`question` = 问答卡 */
export const HEADLESS_PENDING_KINDS = ["approval", "question"] as const;
export type HeadlessPendingKind = (typeof HEADLESS_PENDING_KINDS)[number];

/** 审批决策选项（与 `ApproveOptionsView.options` 同形：id 供应答端点回带、label 供展示）。
 *  id 来自上表（`allow` / `deny`）；**前端只渲染不另编词**。 */
export interface HeadlessApprovalOption {
  id: string;
  label: string;
}

/** 问答卡的一题（Rust `cli_three::Q` 的载荷投影；附录 E-② 的 `{question, header?,
 *  options:[{label,description?}], multiSelect?}` 收敛同形）。
 *
 *  **作答完整性（防静默丢题，附录 E-②/③）**：多选至少一项、单选必选一项或填「其他」自由文本
 *  ——未答全时提交按钮**禁用**并显示未答题面（核侧再拒一次：`AnswerSet::new`）。 */
export interface HeadlessApprovalQuestion {
  /** 题面全文（**就是 answers 的键**——不要用 header / 序号） */
  question: string;
  header?: string | null;
  multiSelect: boolean;
  options: { label: string; description?: string | null }[];
}

/** **无头通道待答请求**（Task 13/C4：claude 的 stdio 双向桥投影成本形状）。
 *
 *  来源：`GET /m/api/v1/session-headless-approval`（`{pending: …}`）。stdout 的
 *  `control_request{can_use_tool}`（工具名 + 入参原文）→ 卡片 → 用户选择 →
 *  `POST /m/api/v1/session-headless-approve` → 后端写 `control_response` 到 claude stdin。
 *
 *  **应答不经前端回带 input**：allow 所需的 `updatedInput`（原 input 原样回显，缺则工具
 *  永不执行）由后端持有原始 input 完成；问答卡的 `answers` 由前端按题面文本上行。
 *
 *  边界：**codex queue 通道没有审批面**（H8 定案），zcode yolo 亦无（裁决 14），
 *  kimi/opencode 的非交互模式由 CLI 自行拒绝权限请求（Task 13 实测）——本形状**只属于
 *  claude 的双向桥**（`channel === "headless_claude_p"`）。 */
export interface HeadlessApprovalPending {
  /** 请求标识（应答须回带同一 id；陈旧页面的 id 会被核侧如实拒绝） */
  requestId: string;
  /** 待答种类（`approval` / `question`） */
  kind: HeadlessPendingKind;
  /** 工具名（如 Bash / AskUserQuestion）——卡片标题 */
  toolName: string;
  /** 入参**展示原文**（Bash = 命令行原文）——卡片主体：用户必须看清要批准什么 */
  input: string;
  sessionId: string;
  /** 无头通道 wire 名（如 `headless_claude_p`） */
  channel: string;
  /** 该通道 **spawn 时选定**的权限档 wire 词（Rust `PermissionSpec::tier()`：claude = `stdio`
   *  = 审批走 stdio 双向桥）。本批只展示，**不做移动端主动切档**（三期 F3.1） */
  tier: string;
  /** claude 的 `--permission-mode` 档（如 `default`）——与 `tier` 并列展示：
   *  `tier` 说「审批面形态」，本字段说「工具审批在 CLI 内部的档」 */
  permissionMode?: string | null;
  /** 审批卡的两枚决策（`kind==="approval"` 时用；词表 = `HEADLESS_DECISION_WORDS` 前两项） */
  options: HeadlessApprovalOption[];
  /** 问答卡的题集（`kind==="question"` 时用；空数组 = 题面缺失，卡片如实说明不编题） */
  questions: HeadlessApprovalQuestion[];
  /** 已等待毫秒（用户能看出回合在等自己多久了） */
  waitedMs: number;
}

/** 拉取**当前待答**的无头审批/问答请求（Task 13/C4）：GET /session-headless-approval。
 *  `{pending: null}` = 没有待答项（回合未到审批点 / 已终结 / 已超时）。
 *  非 2xx（400 缺参 / 403 设备失效）→ 抛 ApiError（调用方静默降级为「无卡」）。 */
export async function fetchHeadlessApproval(
  sessionId: string
): Promise<HeadlessApprovalPending | null> {
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-headless-approval?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-headless-approval 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-headless-approval ${r.status}`);
  const body = (await r.json()) as { pending?: HeadlessApprovalPending | null };
  return body.pending ?? null;
}

/** 应答**在飞无头回合的审批/问答卡**（Task 13/C4）：POST /session-headless-approve。
 *  契约（HTTP 恒 200，语义在 body，与取消端点同规）：
 *  - `{delivered:true}` = 应答已交给回合（回合随后写 `control_response` 到 stdin）；
 *  - `{delivered:false, reason}` = **未送达**（无待答项 / 请求标识不符 / 回合已不再等待）
 *    ——**不是错误**，只是这一答没赶上（回合可能已终结/超时）；前端如实回显并停止轮询。
 *  非 2xx（400 缺参或未答全 / 403 设备失效）→ 抛 ApiError（400 的 body.reason 可直接展示，
 *  如「问答未答全…」——核侧的完整性拒绝）。 */
export async function headlessApprove(
  sessionId: string,
  requestId: string,
  decision: HeadlessDecisionWord,
  answers?: { question: string; labels: string[] }[]
): Promise<{ delivered: boolean; reason?: string }> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-headless-approve", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        sessionId,
        requestId,
        decision,
        ...(answers ? { answers } : {}),
      }),
    });
  } catch (e) {
    throw new ApiError(null, `session-headless-approve 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    // 400 的 body.reason（未答全等）解析进 data 供调用方展示（对齐 sessionSend 惯例）
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 交验失败保持 null */
    }
    throw new ApiError(r.status, `session-headless-approve ${r.status}`, data);
  }
  return (await r.json()) as { delivered: boolean; reason?: string };
}

/** 拉取输入区可用性（W4：输入区挂载时一次）。403（设备失效，与 fetchSessions
 *  同语义）→ null；其余失败（404 会话不在快照 / 网络异常）→ 抛 ApiError，
 *  由调用方静默降级（不渲染输入区，详情页正文照常） */
export async function fetchSendInfo(sessionId: string): Promise<SendInfo | null> {
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-send-info?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-send-info 网络异常: ${String(e)}`);
  }
  if (r.status === 403) return null; // 设备失效 → 回配对页
  if (!r.ok) throw new ApiError(r.status, `session-send-info ${r.status}`);
  return (await r.json()) as SendInfo;
}

/** 发送回执（POST /session-send 响应四态，HTTP 200 恒定，语义在 body.status）：
 *  delivered=已直送终端；submitted=已投递未确认（D7/T3 确认判据收紧，验收问题 #5：
 *  注入 Ok + 戳未中 + 屏读无滞留草稿 = 消息已被 TUI 收进内部队列，agent 空闲后
 *  处理——中性态，非失败、**不提供重试**，重试 = 双发且 TUI 那份无法撤回）；
 *  queued=运行中留队（itemId+position 供插队/撤回/排队指示）；failed=注入失败回执
 *  （error 文案可直接展示；失败行已退出 pending，队列无残留，重按发送即重试） */
export type SendResult =
  | { status: "delivered" }
  | { status: "submitted" }
  | { status: "queued"; itemId: number; position: number }
  | { status: "failed"; error: string }
  /** **无头回合回执**（Task 8 / H7）：`receipt` 是后端 Task 6 归一产物原样透出
   *  （ok|queued|failed|cancelled + stage/reason），`visibilityNote` 是可见性提示逐字文案
   *  （未信任/失败时缺省）。**与终端四态分列**：无头回合没有「入队」概念（每回合 spawn），
   *  故不合成 queued{itemId}/delivered 语义。 */
  | {
      status: "headless";
      channel?: string;
      receipt: HeadlessReceipt;
      visibility?: SendInfo["visibility"];
      visibilityNote?: string;
    };

/** 发送消息（W4 直发/入队分派，后端按输入态路由；多行原样上行，归一在服务端
 *  入队时一次完成）。非 2xx（400 参数非法 / 404 会话消失 / 403 不可注入）→
 *  抛 ApiError。
 *  queueOnly（D6 修改重发，可选）：true = 只入队（后端跳过直发尝试，即使快照显示
 *  可输入也强制入队，防变相插队）；入队后 flush 循环对其与普通队列项同权（转闲
 *  按序自动放行）。**缺省不带该键**——保持既有请求体形态不变（普通发送路径零漂移） */
export async function sessionSend(
  sessionId: string,
  text: string,
  queueOnly?: boolean
): Promise<SendResult> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-send", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, text, ...(queueOnly ? { queueOnly: true } : {}) }),
    });
  } catch (e) {
    throw new ApiError(null, `session-send 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    // 403 not_injectable{reason,reasonCode}（如挂载后会话漂移为 APP/黑盒形态）——
    // 后端已备好中文 reason，解析进 data 供调用方展示（对齐 fetchFile 惯例）
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 交验失败保持 null */
    }
    throw new ApiError(r.status, `session-send ${r.status}`, data);
  }
  return (await r.json()) as SendResult;
}

/** **取消在飞的无头回合**（Task 8 / H4）：POST /session-headless-cancel。
 *  契约（HTTP 恒 200，语义在 body）：
 *  - `{cancelled:true}` = 取消**送达**（先到者生效），回合会以 `cancelled` 回执收尾；
 *  - `{cancelled:false, reason}` = 未送达（无在飞回合 / 回合已终结 / 已被取消）——
 *    **不是错误**，只是这一发取消没赶上；前端照常等回执。
 *  非 2xx（400 缺参 / 403 设备失效）→ 抛 ApiError（调用方静默降级为提示，不阻断回合）。 */
export async function headlessCancel(
  sessionId: string
): Promise<{ cancelled: boolean; reason?: string }> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-headless-cancel", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId }),
    });
  } catch (e) {
    throw new ApiError(null, `session-headless-cancel 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-headless-cancel ${r.status}`);
  return (await r.json()) as { cancelled: boolean; reason?: string };
}

/** 上传附件（2026-09-20）：原始字节 POST 到 /session-attachment——服务端落盘到
 *  **用户项目目录** .tuvis-attachments/<会话>/，返回绝对路径供消息内联标记
 *  （<image|file path>，文件池既有约定）引用。
 *  **上行走 XHR 而非 fetch（Task 10 §C5）**：fetch 规范不暴露上传进度事件
 *  （2026-10-07 全仓核查原零 XMLHttpRequest），大文件在 1–2 Mbps 受限通道上
 *  动辄数分钟，零进度不可接受。`xhr.upload.onprogress` 必须**先于 send() 注册**
 *  （顺序错收不到任何事件）。onProgress 是**第 4 参**——第 3 参是既有 signal
 *  （插错位会被当成 signal，编译期不一定报错、运行时静默失效）。
 *  signal 语义保留：已中止 → 不发请求直接拒绝；中止事件 → xhr.abort()。
 *  错误契约：403 → null（设备失效，与 fetchSendInfo 同口径）；404 →
 *  ApiError(404, "no_session"|"no_cwd")（composer 据后者禁用上传钮）；
 *  413 → ApiError(413, "too_large")；其余非 2xx → ApiError(status) */
export async function uploadAttachment(
  sessionId: string,
  file: File,
  signal?: AbortSignal,
  onProgress?: (loaded: number, total: number) => void
): Promise<{ path: string; size: number } | null> {
  if (signal?.aborted) {
    throw new ApiError(null, "session-attachment 网络异常: AbortError（上传已取消）");
  }
  const q = new URLSearchParams({ session_id: sessionId, name: file.name });
  return new Promise<{ path: string; size: number } | null>((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", `/m/api/v1/session-attachment?${q}`);
    xhr.setRequestHeader("content-type", "application/octet-stream");
    const onAbort = () => xhr.abort(); // 既有取消语义：上传中移除 chip = 中断上传
    signal?.addEventListener("abort", onAbort, { once: true });
    // 终态收尾：摘掉 abort 监听（removeEventListener 幂等；resolve/reject 对已
    // 结算 promise 是 no-op，晚到的 onload/onabort 无需额外旗标）
    const finish = () => signal?.removeEventListener("abort", onAbort);
    // 上传进度（§C5）：先于 send() 注册——顺序错收不到事件
    xhr.upload.onprogress = (e) => {
      onProgress?.(e.loaded, e.total);
    };
    xhr.onload = () => {
      finish();
      if (xhr.status === 403) return resolve(null); // 设备失效 → 回配对页
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          resolve(JSON.parse(xhr.responseText) as { path: string; size: number });
        } catch {
          reject(new ApiError(xhr.status, `session-attachment ${xhr.status}`));
        }
        return;
      }
      // 非 2xx：错误码在响应体 error 字段（no_cwd / too_large 等），对齐 fetch 版
      let reason = `session-attachment ${xhr.status}`;
      try {
        const j = JSON.parse(xhr.responseText) as { error?: unknown };
        if (typeof j?.error === "string") reason = j.error;
      } catch {
        /* 响应体非 JSON：保留默认 reason */
      }
      reject(new ApiError(xhr.status, reason));
    };
    xhr.onerror = () => {
      finish();
      reject(
        new ApiError(
          null,
          `session-attachment 网络异常: ${String(xhr.statusText || "network error")}`
        )
      );
    };
    xhr.onabort = () => {
      finish();
      reject(new ApiError(null, "session-attachment 网络异常: AbortError（上传已取消）"));
    };
    xhr.send(file); // File 直传——XHR 原生跟踪 Blob 的上传字节
  });
}

/** 排队条目视图（GET /session-queue 的 items 元素，camelCase 契约）：position =
 *  1 起队位；content 为入队时 compose 完成的最终注入文本（含设备名前缀） */
export interface QueueItemView {
  id: number;
  content: string;
  enqueuedAt: number;
  position: number;
}

/** 拉取该会话待发队列（FIFO，W4 排队指示/轮询刷新的数据源）。非 2xx → 抛 ApiError */
export async function fetchQueue(sessionId: string): Promise<QueueItemView[]> {
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-queue?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-queue 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-queue ${r.status}`);
  const j = (await r.json()) as { items?: QueueItemView[] };
  return Array.isArray(j.items) ? j.items : [];
}

/** 插队直发（裁决 12）：按 itemId 点名该会话 pending 中的一条即刻注入。
 *  回执五态精确映射（F7④ + D7/T3 与后端契约对齐）：Sent → delivered；Failed(e) →
 *  failed{error}（注入失败行已退出 pending，可重发）；submitted → submitted
 *  （防御性契约对齐：Submitted 仅直发分诊产出，插队以占用排空定论、后端本臂实际
 *  不可达——前端按非 delivered 走对账收敛即可）；Deferred | Suspended →
 *  queued{itemId,position}（行保持 pending，语义即排队）；守卫忙（该会话
 *  in-flight 投递占用）→ queued{itemId,position}（F1 新语义：旧忙时回 failed
 *  逼客户端重试，现改 queued 让位给进行中的投递）。非 2xx（404 not_found
 *  条目已不在队）→ 抛 ApiError */
export type QueueJumpResult =
  | { status: "delivered" }
  | { status: "submitted" }
  | { status: "queued"; itemId: number; position: number }
  | { status: "failed"; error: string };

export async function queueJump(sessionId: string, itemId: number): Promise<QueueJumpResult> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-queue/jump", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, itemId }),
    });
  } catch (e) {
    throw new ApiError(null, `session-queue/jump 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-queue/jump ${r.status}`);
  return (await r.json()) as QueueJumpResult;
}

/** 撤回排队条目（W4）：200 {ok:true} 撤回成功；P2-6 忙时（该会话投递进行中）
 *  → 200 {status:"failed",error:后端中文文案}（条目**未被撤**、仍在队——前端不
 *  消费该文案，以 fetchQueue 复核结果为准）；条目已不在队（已送达 / 他端撤回）
 *  → 404 not_found → 抛 ApiError（调用方按「已不在队列」收敛，不作失败提示） */
export async function queueRetract(
  sessionId: string,
  itemId: number
): Promise<{ ok: true } | { status: "failed"; error: string }> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-queue/retract", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, itemId }),
    });
  } catch (e) {
    throw new ApiError(null, `session-queue/retract 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-queue/retract ${r.status}`);
  return (await r.json()) as { ok: true } | { status: "failed"; error: string };
}

// ==== M8 Task 12：审批选项卡（红卡一键应答，W6）====

/** 审批选项视图（GET /session-approve-options 载荷，与 Rust `session_approve_options`
 *  的 JSON 逐字段对应，勿漂移）：available=false（会话非 Waiting / 工具无映射 /
 *  提示未命中）时 options 恒空——移动端据此不渲染审批卡；options 只含 id+label，
 *  **键位不外泄给 UI**（投递层机密）；verifiedWith = 映射实测版本，currentVersion =
 *  CLI 探测版本（探测失败为 null），drift=true 时红卡提示降级路径（普通发送）；
 *  reason = 严格档降级原因（Task 10 下发，如「键位待实测确认，请用普通发送」）：
 *  available=false 且 reason 存在 → 卡片只渲染提示条不渲染按键（M9R 消费）；
 *  旧分支（非 Waiting / 无映射 / 未命中）不给该键 → optional */
export interface ApproveOptionsView {
  available: boolean;
  options: { id: string; label: string }[];
  verifiedWith: string;
  currentVersion: string | null;
  drift: boolean;
  reason?: string;
  /** 批次丙 T8：审批点 plan 聚合——计划确认类审批卡主体带计划全文（claude/codex
   *  的 kind="plan" 消息）或计划文件路径（kimi 的 kind="plan-file"，isFile=true
   *  → 前端走文件预览读全文）。null/缺省 = 无计划在场（只渲染选项） */
  plan?: { content: string; isFile: boolean } | null;
  /** 批次丙 R1-3：**降级警示**——命中审批但未读到终端对话框选项（终端可能正显示
   *  多选项，二元键可能错位）。前端在二元卡渲染脚注。null/缺省 = 未降级 */
  degradedHint?: string | null;
  /** 批次丙 T5：选项来自**对话框屏读**——id 形如 `dialog:<n>`，label 是屏上原文
   *  （如 "1. Yes, and use auto mode"）；前端据此渲染编号按钮组（点按注入数字键 n）。
   *  缺省/ false → 映射表二元项（既有渲染，前向兼容旧后端） */
  dialog?: boolean;
  /** 丁T2：**计划待确认预期态**（消息尾部派生，无新存储）——codex/kimi 的计划确认框
   *  不落状态/标记，这是它唯一的可见信号。true 且 `dialog=false` 时前端渲染
   *  「计划待确认」条 +「检查终端对话框」按钮（点它重拉本端点；后端屏读命中即出
   *  N 选项）；此形态下 options 恒空**不是错误**，是「还没读到选项，点检查重试」。
   *  缺省/false → 既有渲染（前向兼容旧后端） */
  planPending?: boolean;
  /** 2026-10-04 计划批准卡：屏上选项带「告诉 Claude 要改什么」锚 → claude 计划
   *  批准框（前端渲染「计划批准」标题与反馈入口）。缺省/false → 普通审批对话框。 */
  planDialog?: boolean;
  /** 反馈选项 id（如 "dialog:3"；null/缺省 = 非计划批准框）——前端把该选项渲染为
   *  「告诉 Claude 要改什么」入口，点击走 /session-plan-feedback 的 start 动作
   *  （选中该选项进入反馈编辑态），而非直发按键。 */
  feedbackOption?: string | null;
}

/** 拉取审批选项卡数据源（红卡挂载时一次）。非 2xx → 抛 ApiError（调用方静默
 *  降级不渲染，与 fetchSendInfo 失败静默同惯例） */
export async function fetchApproveOptions(sessionId: string): Promise<ApproveOptionsView> {
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-approve-options?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-approve-options 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-approve-options ${r.status}`);
  return (await r.json()) as ApproveOptionsView;
}

/** 审批应答回执（POST /session-approve 响应，HTTP 200 恒定，语义在 body.status）：
 *  key_sent=按键已投递终端；failed=投递失败 / in-flight 忙让位（error 为后端中文
 *  文案，如「该会话投递进行中，请稍后重试」，可重试） */
export type ApproveResult = { status: "key_sent" } | { status: "failed"; error: string };

/** 审批一键应答（M8 红卡）。200 {status:"key_sent"} | 200 {status:"failed",error}；
 *  409 {error:"not_waiting"} | 404 {error:"no_mapping"|"no_session"} → 非 2xx 抛
 *  ApiError（错误码解析进 data.error，调用方分診中文文案——not_waiting 已不在
 *  等待、no_mapping 降级走普通发送） */
export async function sessionApprove(sessionId: string, optionId: string): Promise<ApproveResult> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-approve", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, optionId }),
    });
  } catch (e) {
    throw new ApiError(null, `session-approve 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    // 409/404 错误码在响应体 data.error——解析进 data 供调用方分診（对齐 sessionSend 惯例）
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体（代理注入页等）：data 保持 null，按 message 兜底 */
    }
    throw new ApiError(r.status, `session-approve ${r.status}`, data);
  }
  return (await r.json()) as ApproveResult;
}

// ==== 2026-10-04 计划批准卡：计划反馈通道（claude 计划批准框选项 3）====

/** 计划反馈动作（claude 计划批准框选 3 之后的 composer 通道；「选 3」本身走
 *  session-approve 的 dialog:3——探测定案 2026-10-04 §S3）：
 *  - `type`：清空编辑行（有内容时）后打字（`submit=true` 尾随 Enter 提交——Claude
 *    留在计划模式开启新一轮修改；`submit=false` 仅暂存，供「覆盖写入」改错字）；
 *    text 为空且 submit=true = 仅回车（提交终端里已暂存的内容）；
 *  - `clear`：退格清空编辑行（放弃暂存内容）。 */
export type PlanFeedbackAction = "type" | "clear";

/** 计划反馈回执：editor_ready = 反馈编辑态已进入（start）；done = 动作已投递
 *  （type/clear）；failed = 投递失败 / 状态不符（error 为后端中文文案，可重试） */
export type PlanFeedbackResult =
  { status: "editor_ready" | "done" } | { status: "failed"; error: string };

/** 计划反馈（POST /session-plan-feedback）。非 2xx 抛 ApiError（错误码在 data.error，
 *  调用方分診中文文案——not_waiting / no_session 等，与 sessionApprove 同惯例） */
export async function sessionPlanFeedback(
  sessionId: string,
  action: PlanFeedbackAction,
  opts: { text?: string; submit?: boolean } = {}
): Promise<PlanFeedbackResult> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-plan-feedback", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, action, text: opts.text, submit: opts.submit }),
    });
  } catch (e) {
    throw new ApiError(null, `session-plan-feedback 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体（代理注入页等）：data 保持 null，按 message 兜底 */
    }
    throw new ApiError(r.status, `session-plan-feedback ${r.status}`, data);
  }
  return (await r.json()) as PlanFeedbackResult;
}

// ==== 批次乙 T8：问答卡（AskUserQuestion，claude 先行）====

/** 问答选项视图（questions[].options[] 条目）：label + description——**编号是渲染层
 *  按 index 生成**，键位/数字不在此列（投递层细节不外泄 UI，approve 同纪律）；
 *  description 恒在（后端 json! 无条件输出，缺省解析为空串）→ 必填 string */
export interface QuestionOptionView {
  label: string;
  description: string;
}

/** 问答题目视图（GET /session-question 载荷 questions[] 条目，与 Rust
 *  `session_question` 的 JSON 逐字段对应，勿漂移）：multiSelect=false → 单选，
 *  点选项=直接提交；true → 多选，点选=勾选切换 + 「提交」钮。questions.length>1
 *  → 前端按只读卡渲染（多问题翻页键序未测，不做注入） */
export interface QuestionView {
  header?: string;
  question: string;
  multiSelect: boolean;
  options: QuestionOptionView[];
}

/** 问答可用性视图（GET /session-question 载荷）：available=false（双通道均未命中 /
 *  审批标记隔离 / 会话不在快照）→ questions 恒空——移动端据此不渲染问答卡；
 *  answerable=false（T3：该工具的问答键序未实测，如 codex）→ 渲染**只读卡** +
 *  引导终端作答，不显示可点选项（「未验不出键」）；source = 识别通道诊断
 *  （"mark"=hook 标记〔通道 A〕/"scan"=会话消息兜底〔通道 B〕） */
export interface QuestionInfoView {
  available: boolean;
  /** 可选：旧后端不带该字段时按 true 处理（前向兼容——只有明确 false 才降只读） */
  answerable?: boolean;
  /** 丁T5 §2.4：卡内自由作答输入框是否可用（**独立于 `answerable`**——
   *  codex/opencode 的**点选**已实测可作答，但**自由作答序列未定案**）。
   *  缺省/旧后端 → 按 false 处理：渲染「请在终端作答」引导文案，**不假装能发**。 */
  freeText?: boolean;
  /** 批次戊 E4-E6：多题交互能力（kimi/codex/opencode 已实机定案）——true 时多题卡
   *  渲染逐题作答 UI；缺省/旧后端/false → 只读卡（红线不变） */
  multiQuestion?: boolean;
  /** 切换题目能力（2026-09-23 错位修复）：多题卡多选题的显式切页动作——仅 opencode
   *  （tab=前向切页，实测定案）。缺省/旧后端 → 按 false：多选题不渲染「切换题目」钮，
   *  改渲染「请到终端切题」引导（不假装能发）。 */
  advance?: boolean;
  /** **←/→ 双向导航**（2026-10-02）：仅 claude 的 ←/→ 键序已活体取证（含 Review
   *  导航环）。true → 题卡渲染 ◀ 上一题/下一题 ▶ 双钮（单选/多选都渲染）、确认卡
   *  「返回上一题修改」发 prev；false（opencode/旧后端）→ 维持旧单钮 + tab 回绕。
   *  缺省 → 按 false 处理（前向兼容）。 */
  navBoth?: boolean;
  /** **多选卡自由作答**（2026-10-04 opencode own answer 接入）：claude/opencode =
   *  true（opencode 的 toggle 双 enter 语义编排已定案）；kimi/codex 未取证恒 false。
   *  缺省 → 按 false 处理（旧后端前向兼容）。 */
  multiFreeText?: boolean;
  /** **覆盖写入/清空能力位**（2026-10-05 深夜）：键序语义逐工具实机取证后才渲染
   *  这两个按钮——opencode 已取证（enter 探针走位 + 退格清空闭环）；codex notes
   *  覆盖语义未取证、kimi 多选自由作答未接入 → false（缺省 false，旧后端兼容） */
  freeTextOverwrite?: boolean;
  /** **屏读快照**（2026-10-03 卡面状态权威源）：GET 时终端若停在题屏 →
   *  {heading, checked, freeText}（前端据此对位当前题并纠偏 mqIndex/勾选/输入框）；
   *  停在 Review 确认屏 → {review:true}（前端直接进确认卡）；null = 屏读不可用/
   *  非题屏（前端维持本地状态）。codex（2026-10-09，0.160.0）形状不同：
   *  题号对位主键 questionIdx + 未答计数（见下方 codex 字段组）。 */
  screen?: {
    review?: boolean;
    /** Review 页逐题摘要（2026-10-07 确认卡权威源切换：Q/→ 行解析，以终端为准） */
    summary?: { q: string; a: string }[];
    heading?: string;
    checked?: (boolean | null)[];
    freeText?: string | null;
    freeTextPresent?: boolean;
    /** codex 面板形状（2026-10-09 设计 §3.1）：题号对位主键 + 未答驱动进度提示 */
    questionIdx?: number;
    questionTotal?: number;
    unanswered?: number;
    isLast?: boolean;
    options?: string[];
    focused?: number | null;
    /** codex notes 行文本（2026-10-10 用户指令：note 位置但凡有输入必须显示在
     *  远端页面上——GET 屏读同帧解析，占位/无备注 → null） */
    noteText?: string | null;
  } | null;
  questions: QuestionView[];
  source?: "mark" | "scan" | null;
}

/** 拉取问答卡数据源（卡片挂载时一次）。非 2xx → 抛 ApiError（调用方静默降级
 *  不渲染，fetchApproveOptions 失败静默同惯例） */
export async function fetchSessionQuestion(sessionId: string): Promise<QuestionInfoView> {
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-question?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-question 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-question ${r.status}`);
  return (await r.json()) as QuestionInfoView;
}

/** 问答应答动作：select=单选点选项（数字直接提交）；toggle=多选勾选切换（**claude
 *  走闭环阶段机**：屏读定位 → 空格 → 屏读校验翻转，2026-09-24 数字路径废止）；
 *  submit=多选提交（**阶段机闭环**：屏读确认每段后才推进）；
 *  cancel=取消问题（Esc）；freeText=自由作答（**仅 claude**，阶段机闭环：
 *  定位 `Type something` 行 → 文本 → 回车）；
 *  advance=多题切换题目（opencode=tab 前向切页；claude=**←/→ 双向导航**
 *  `direction` 指定上一题/下一题——读屏分类题干区变化，均纯导航，不触碰勾选态）。 */
export type QuestionAnswerAction =
  "select" | "toggle" | "submit" | "cancel" | "freeText" | "advance";

/** 阶段机动作的**段名**（回执 `stage` 字段的取值；与后端
 *  `remote::api::QUESTION_STAGE_*` 常量逐字对应，勿漂移）。
 *  提交链推进序：`submit-row`→`review`→`confirm`→`receipt`；
 *  自由作答：`free-row`→`free-text`；claude 切勾链：`toggle-row`；切题：`advance`；
 *  `select`：单选定位/选择段（opencode 焦点守卫 + **codex select 链全部中止**——
 *  2026-10-09 后端 `stage_from_abort` 的 codex 映射臂统一落此段名）。 */
export type QuestionAnswerStage =
  | "submit-row"
  | "review"
  | "confirm"
  | "receipt"
  | "free-row"
  | "free-text"
  | "toggle-row"
  | "advance"
  | "select";

/** 问答应答回执（POST /session-question/answer 响应，HTTP 200 恒定，语义在 body.status）。
 *
 *  **丁T5 起 status 仍是既有两词**（`key_sent` / `failed`），新增字段全部是**附加**
 *  ——故旧前端（只读 status）行为不变：
 *  - `key_sent`：按键已投递。**单键动作**（select/toggle/cancel）到此为止；
 *    **阶段机动作**（submit/freeText/claude 的 toggle）走完整条闭环时带 `done:true` +
 *    `stage`（走完的段）+ `verified`（终态回执三态：true=屏读到终态锚；false=读到屏
 *    但未见锚；null/缺省=读屏不可用。**false 与 null 都不是「失败」，是「未确认」**）；
 *    **claude 的 toggle** 另带 `checked`（屏读核验到的目标行新勾选态——终端真值，
 *    卡面以它为准同步；null/缺省=无法核验，卡面回落盲翻）；
 *  - `failed`：投递失败 / in-flight 忙让位 / **阶段机中止**。`aborted:true` + `stage`
 *    标记后者（`error` 是带段名的中文文案，用户可读）。 */
export type QuestionAnswerResult =
  | {
      status: "key_sent";
      done?: boolean;
      stage?: QuestionAnswerStage;
      verified?: boolean | null;
      checked?: boolean | null;
      /** claude 切题（2026-09-24；2026-10-02 ←/→ 双向）：终端是否已切题。`false` =
       *  下一题请求但已在 Review 确认屏、零按键（已在终点）——前端**不**推进。
       *  缺省（opencode 等旧路径）视为已前移（既有行为不变） */
      advanced?: boolean;
      /** 保存后 TUI 直达 Review/Submit 汇总屏（末题/全部已答自动汇总）——前端据此切确认卡 */
      review?: boolean;
      /** claude 切题（2026-10-02）：方向 echo（prev/next）——前端移动 mqIndex 需
       *  确认 echo 与请求 direction 一致（opencode 旧回执无此字段 → 维持回绕行为） */
      direction?: string;
      /** claude 交互回执的**屏读快照**（2026-10-03 屏读为准）：toggle=切勾后整屏、
       *  select=发后整屏、advance=新题屏——TS 行内容/勾选态随回执回传（交互后核对）。
       *  codex 回执（CodexSelectDone/CodexAdvanceDone）的 screen 同为 codex 面板
       *  形状（GET 的 screen 用 questionIdx 直读对位；回执 screen 同形状——
       *  advance/select 路径的消费见 QuestionCard） */
      screen?: {
        heading: string;
        checked: (boolean | null)[];
        freeText: string | null;
        freeTextPresent?: boolean;
        /** codex 面板形状（2026-10-09 设计 §3.1）：题号对位主键 + 未答驱动进度提示 */
        questionIdx?: number;
        questionTotal?: number;
        unanswered?: number;
        isLast?: boolean;
        options?: string[];
        focused?: number | null;
        /** codex notes 行文本（2026-10-10，与 GET screen 同名同义） */
        noteText?: string | null;
      };
      /** codex select 回执旗标（2026-10-09 设计 §3.7 wire 契约）：
       *  `reanswered` = 改答分支（题号推进而计数未减）；`alreadySubmitted` =
       *  双条件闸读到面板消失且未发提交键（如 notes 回车连带交卷）。
       *  前端当前仅透传展示或暂不消费——先补契约（与后端 json! 逐字对应）。 */
      reanswered?: boolean;
      alreadySubmitted?: boolean;
      /** claude 多选自由作答（2026-10-03 屏读为准）：该行的屏上文本（勾选态复用
       *  上方 `checked` 字段——与 toggle 回执同字段同语义）；null = 收尾读屏失败
       *  （未核验，前端不得虚报已写入） */
      text?: string | null;
    }
  | { status: "failed"; error: string; aborted?: boolean; stage?: QuestionAnswerStage };

/** 问答应答**错误码 → 用户可读中文文案**（丁T6 复评抽出：卡内与 composer 两条入口
 *  必须**同口径**——两处各写一套 `if/else` 迟早漂移，且 composer 侧曾漏掉这条映射
 *  （读的是 `data.reason` 而问答端点回的是 `data.error`）→ 409 会显示成
 *  「session-question/answer 409」这种对用户无意义的串）。
 *
 *  取值来源：后端 `remote::api::session_question_answer` 的 409/400 错误码
 *  （`no_question` / `multi_questions` / `tool_readonly` / `bad_index`；
 *  另有 `bad_request` 兜底）。码不在表内 → 回原 message（不编文案）。 */
export function questionAnswerErrorCopy(e: ApiError): string {
  const code = typeof e.data?.error === "string" ? e.data.error : null;
  if (code === "no_question") return "当前没有待回答的问题";
  if (code === "multi_questions") return "多个问题请回到终端完成作答";
  if (code === "tool_readonly") return "该工具的远程作答尚未实测，请在终端完成作答";
  if (code === "bad_index") return "选项序号无效，请刷新后重试";
  return e.message;
}

/** 问答一键应答（T8；丁T5 起支持 freeText）。index = 选项序号（0 起；select/toggle
 *  必填）；text = 自由作答正文（freeText 必填；后端归一后走**字符通道**注入，
 *  不带 `[mobile]` 签名）。
 *  409 {error:"no_question"|"multi_questions"|"tool_readonly"} |
 *  400 {error:"bad_request"|"bad_index"}
 *  → 非 2xx 抛 ApiError（错误码解析进 data.error，调用方分診中文文案——
 *  用 [`questionAnswerErrorCopy`]，勿另写一套） */
export async function sessionQuestionAnswer(
  sessionId: string,
  action: QuestionAnswerAction,
  index?: number,
  text?: string,
  /** 批次戊 E4-E6 多题交互：select/toggle 作用在第几题（0 起） */
  questionIndex?: number,
  /** claude 切题方向（2026-10-02 ←/→ 双向导航；仅 advance 消费，缺省 next） */
  direction?: "prev" | "next",
  /** 覆盖写入（2026-10-02 多选自由作答编辑；仅 freeText 消费）：true = 该行已有
   *  内容时先退格清空再打新字；缺省 false = 已有内容即中止 */
  overwrite?: boolean
): Promise<QuestionAnswerResult> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-question/answer", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, action, index, text, questionIndex, direction, overwrite }),
    });
  } catch (e) {
    throw new ApiError(null, `session-question/answer 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    // 409/400 错误码在响应体 data.error——解析进 data 供调用方分診（sessionApprove 惯例）
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体：data 保持 null，按 message 兜底 */
    }
    throw new ApiError(r.status, `session-question/answer ${r.status}`, data);
  }
  return (await r.json()) as QuestionAnswerResult;
}

// ==== 批次丙 T6：模式切换 ====

/** 统一模式档（与 Rust `inject::mode::MamMode` 的 wire 词一一对应，勿漂移）。
 *  对齐 happy 的 8 值收敛为 兔维斯 5 值（auto/safe-yolo/yolo 合并为 bypass）。 */
export type MamMode = "plan" | "default" | "acceptEdits" | "bypass" | "readOnly";

// 注（T4 复评 M4）：批次丙 T6 的 `MAM_MODE_LABELS` 通用档名表已删除——丁T4 起
// 按钮标签一律用**后端下发的屏显标签**（`groups[].tiers[].label`，§2.6：标签必须
// 是工具自己的词，如 kimi 权限组的「总是询问/按需询问/永不询问」），通用档名只剩
// 回执文案里的兜底（`MamMode::label`，后端侧）。前端再留一份 = 第二份真源 + 死代码。

/** 模式栏的**组**（丁T4 §2.6：二维工具的「模式组/权限组」与单轴工具的「模式」轴） */
export type ModeGroupId = "mode" | "permission";

/** 单档（GET 载荷 `groups[].tiers[]`）：屏显标签来自**工具自己的词表**（§2.6
 *  「档位（屏显标签）」列）——kimi 权限组是「总是询问/按需询问/永不询问」，不是 兔维斯
 *  通用名（用户看到的是终端上的词，对不上号等于没回显）。
 *  `selectable=false` → 不渲染为可点按钮（`reason` 是后端给出的如实原因）。 */
export interface ModeTierView {
  mode: MamMode;
  label: string;
  selectable: boolean;
  reason?: string | null;
}

/** 已退役旧档（裁7）：**不可选**，只作如实展示（codex 的 untrusted / on-failure） */
export interface ModeLegacyView {
  label: string;
  note: string;
}

/** 单组（GET 载荷 `groups[]`）。`step=true` = 步进轴（shift+tab 一次一档，档位顺序即
 *  实测环序）；`readback=false` = 该组无屏读源（前端必须显示「请人工核对」）。
 *  `current=null` = 档未知（屏读失败或该组无回读源）→ **不得假装知道**（红线 4）。
 *  `layout`（2026-09-23 codex 模式切换改造；2026-10-10 用户指令改版）：
 *  `"toggle"` = codex 模式组三段 [计划] [◀▶] [操作]——两个标签是指示器（当前档高亮），
 *  中间按钮点击向终端发一次 shift+tab（无零投递闸，卡面过期也必然动作；target 由前端
 *  按当前档翻转、仅作后端前读不可判时的核验预期兜底）；缺省/`"tiers"` = 逐档按钮；
 *  `"picker"` = codex 权限组——四档 chips 常驻直选（生效档高亮框选中，点 chip 走
 *  switch 端点 Menu 编排）+「切换权限」单选面板兜底（后端读回**终端菜单的选项表**，
 *  编号 = 屏上实读值，兔维斯 只投递不猜）。
 *  `currentSource`（终审 P1-2）：`current` 的来源——`"screen"` = 终端屏读（实时权威）/
 *  `"memory"` = 「上次切换」记忆回落（终端手改会失真，前端标注「（上次切换）」明示
 *  口径）/ `"null"` = 未知。旧后端无此字段（undefined = 不标注）。 */
export interface ModeGroupView {
  id: ModeGroupId;
  label: string;
  step: boolean;
  readback: boolean;
  layout?: "tiers" | "toggle" | "picker";
  current: MamMode | null;
  currentLabel: string | null;
  currentSource?: "screen" | "memory" | "null";
  tiers: ModeTierView[];
  legacy?: ModeLegacyView[];
}

/** 模式视图（GET /session-mode 载荷）。current=null 表示**档未知**（屏读失败或该
 *  工具不支持回显）→ 前端必须显示「未知」并要求人工核对（红线 4：不假装成功）。
 *  switchKind：unsupported → 不显示切换按钮（该工具无实测机制）。
 *
 *  丁T4 增量（**全部可选**，与旧后端前向兼容）：`structure`/`groups` 缺失时前端回落
 *  到「单轴渲染 + 顶层 current」。 */
export interface SessionModeView {
  tool: string;
  current: MamMode | null;
  currentLabel: string | null;
  readback: boolean;
  switchKind: "shiftTab" | "slashCommand" | "unsupported";
  /** E3④：终端问答待决（消息尾部形态）——切档注入含回车会被问答框误消费
   *  （codex 交默认答案 / kimi 误选推进待决态）→ 前端置灰按钮 + 原因文案。
   *  旧后端无此字段（undefined = 未知，不置灰——与「无法判定放行」同一取向）。 */
  questionPending?: boolean;
  /** "twoAxis" | "singleAxis" | "none"（旧后端无此字段） */
  structure?: "twoAxis" | "singleAxis" | "none";
  groups?: ModeGroupView[];
}

/** 切档回执（POST /session-mode/switch）。verified=false 时 hint 给出人工核对提示
 *  ——切换已投递但无法自动验证（屏读缺失），前端据此渲染提示而非「已切到 X 档」。
 *  丁T3：`dialogChecked` = 本次是否真的做过对话框在场检测（false = 平台无屏读或
 *  屏读失败；此时守卫按「无法判定」放行，前端不得声称已检查）。
 *  丁T4：`current`/`currentLabel` = 投递后回读到的档（命中时前端可直接用它刷新）。
 *  T5（spec §6-T5，m-6）：`observed` = codex 模式组 toggle 臂的**核验末拍屏读档**
 *  （MamMode wire 词，与 `current` 同形同源；null = 不可判）。前端**不拿它改本地
 *  状态**（卡面以重拉 GET 的权威结构为准，丁T4 纪律）；verified=false 时其档名已
 *  并入后端 hint 文案（「屏已切换至 X（预期 Y）」），前端不重复拼接。
 *  注（2026-10-10 用户指令）：「已在目标档」零投递态已删除——点切换必然发键，
 *  后端 hint 只剩命中/不符/未生效三态，wire 从未携带 `zeroKey` 字段。 */
export type SessionModeSwitchResult =
  | {
      status: "key_sent";
      verified: boolean;
      hint?: string | null;
      dialogChecked?: boolean;
      current?: MamMode | null;
      currentLabel?: string | null;
      observed?: MamMode | null;
    }
  | { status: "failed"; error: string; dialogChecked?: boolean };

/** 拉取当前模式（卡头显示用）。非 2xx → 抛 ApiError（调用方静默降级不显示） */
export async function fetchSessionMode(sessionId: string): Promise<SessionModeView> {
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-mode?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-mode 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-mode ${r.status}`);
  return (await r.json()) as SessionModeView;
}

/** 切档（T6；丁T4 加 `group`）。404 no_session | 409 no_mechanism |
 *  **409 blocked_by_dialog**（丁T3 §2.7 对话框在场红线：控制类注入被拒，data.reason
 *  为中文文案）→ 非 2xx 抛 ApiError。
 *
 *  `group` 是**可选**参数（丁T4）：不传 = 由后端按 target 归组（旧客户端的调用面，
 *  语义见 Rust `inject::mode::resolve_group`）；二维工具（codex/kimi）的新前端传它。 */
export async function sessionModeSwitch(
  sessionId: string,
  target: MamMode,
  group?: ModeGroupId
): Promise<SessionModeSwitchResult> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-mode/switch", {
      method: "POST",
      headers: { "content-type": "application/json" },
      // group 缺省时**不发字段**（旧后端不认识它，发了也只是被 serde 忽略——
      // 但省掉字段可让请求体与旧版本逐字一致，便于抓包比对）
      body: JSON.stringify(
        group === undefined ? { sessionId, target } : { sessionId, target, group }
      ),
    });
  } catch (e) {
    throw new ApiError(null, `session-mode/switch 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体 */
    }
    throw new ApiError(r.status, `session-mode/switch ${r.status}`, data);
  }
  return (await r.json()) as SessionModeSwitchResult;
}

// ==== 2026-09-23：codex 权限组的「终端菜单单选题」（用户方案）====
// **前端消费方状态（2026-10-10 二版）**：唯一 UI 消费方 ModeBar 的 PermissionPicker
// 已随「四 chips 直选」改版删除（评审 P2-2）——以下导出降级为 /session-mode/menu
// 活端点的 TS 契约镜像（后端 src-tauri remote/api.rs 的 menu 端点仍在、switch 端点
// 的 Menu 编排不经过这些函数）。恢复 picker 或端点退役时随批处置，勿在别处新消费。

/** 终端菜单里的一项。`number` = **屏上实读的编号**（用户点它 → 兔维斯 敲同一个数字键）；
 *  `label` = 屏上原文（原样展示，供用户与终端核对）；`highlighted` = 终端当前高亮项。 */
export interface ModeMenuOption {
  number: number;
  label: string;
  highlighted: boolean;
}

/** 终端菜单面板的载荷（POST /session-mode/menu 的 `open`/`pick`，
 *  与 GET /session-mode/menu 的重读同形）。
 *
 *  - `menu`：菜单开着，`options` = 档位表（用户点选）；
 *  - `confirm`：Full Access 的**二阶段风险确认框**在屏，`options` = 确认框选项
 *    （空数组 + `hint` = 确认框在屏但选项未读到，提示重读）；
 *  - `done`：已投递且无确认框。`verified` = 屏上是否读到成功回执行；
 *    `hint` 原样带出后端文案（含回执行原文）——**不假装成功**（目标档未知：
 *    用户点的是屏上编号，后端不知道对应哪个 wire 档，故只报「有没有回执」）；
 *  - `confirm-cancelled`：确认框阶段点了 Cancel（2）→ 确认框消失、**菜单已回到
 *    终端屏上**、无新回执行（T5 消费，spec §3.2/§6-T5）——前端显示 hint 并自动
 *    重拉菜单选项表（既有 open 流程），面板回到菜单态；
 *  - `none`：屏上无菜单/确认框（仅 GET 重读会给）；
 *  - `failed`：如实失败文案。 */
export type ModeMenuResult =
  | { status: "menu"; options: ModeMenuOption[]; dialogChecked?: boolean }
  | {
      status: "confirm";
      options: ModeMenuOption[];
      hint?: string | null;
      dialogChecked?: boolean;
    }
  | {
      status: "done";
      verified: boolean;
      hint?: string | null;
      dialogChecked?: boolean;
    }
  | {
      status: "confirm-cancelled";
      hint?: string | null;
      dialogChecked?: boolean;
    }
  | { status: "none"; options: ModeMenuOption[] }
  | { status: "failed"; error: string };

/** 面板错误体（非 2xx）→ 解析进 ApiError.data（错误码分诊：no_session / no_mechanism
 *  / blocked_by_dialog），与 `sessionModeSwitch` 同口径。 */
async function modeMenuFetch(init: RequestInit, path: string): Promise<ModeMenuResult> {
  let r: Response;
  try {
    r = await fetch(path, init);
  } catch (e) {
    throw new ApiError(null, `session-mode/menu 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体 */
    }
    throw new ApiError(r.status, `session-mode/menu ${r.status}`, data);
  }
  return (await r.json()) as ModeMenuResult;
}

/** **打开终端权限菜单**并读回选项表（注入 `/permissions` + 回车）。
 *  非 2xx → ApiError（409 blocked_by_dialog 等，与切档端点同分诊）。 */
export async function sessionModeMenuOpen(sessionId: string): Promise<ModeMenuResult> {
  return modeMenuFetch(
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, action: "open" }),
    },
    "/m/api/v1/session-mode/menu"
  );
}

/** **按用户点选的屏上编号敲键**（无回车）。硬前置由后端把关：屏上没有菜单/确认框 →
 *  零投递并如实报错（`failed` 或抛 ApiError）。 */
export async function sessionModeMenuPick(
  sessionId: string,
  number: number
): Promise<ModeMenuResult> {
  return modeMenuFetch(
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, action: "pick", number }),
    },
    "/m/api/v1/session-mode/menu"
  );
}

/** **重新读取**（纯屏读、零注入）：把面板与终端当前屏面对齐。用于用户手动在终端开了
 *  菜单、或上一步读屏竞态时。 */
export async function fetchSessionModeMenu(sessionId: string): Promise<ModeMenuResult> {
  const q = new URLSearchParams({ session_id: sessionId });
  return modeMenuFetch({ method: "GET" }, `/m/api/v1/session-mode/menu?${q}`);
}

// ==== M6R–M9R Task 11：一键 resume（R5，在电脑上打开）====

/** 一键 resume 回执（POST /session-open）：200 {status:"opening"} 表示电脑侧正在
 *  打开终端恢复该会话；`trustPromptExpected`（T3）：会话目录命中 ~/.claude.json
 *  未信任条款——claude 将在终端弹信任提示，需人工应答否则会话挂起（调用方据此
 *  toast 提醒）；spawn 出手失败 → 200 {status:"failed",error}（可重试） */
export type SessionOpenResult =
  { status: "opening"; trustPromptExpected?: boolean } | { status: "failed"; error: string };

/** 一键 resume（R5）：请求电脑本机打开终端 + cd 项目目录 + 恢复会话 + 聚焦。
 *  404 {error:"no_session"|"no_cwd"|"no_resume_command"} → 非 2xx 抛 ApiError
 *  （错误码解析进 data.error，调用方分診禁用/失败文案——后端命令表未收录的工具
 *  前端按钮本就禁用，404 是挂载后会话漂移的兜底） */
export async function sessionOpen(sessionId: string): Promise<SessionOpenResult> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-open", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId }),
    });
  } catch (e) {
    throw new ApiError(null, `session-open 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    // 404 错误码在响应体 data.error——解析进 data 供调用方分診（对齐 sessionApprove 惯例）
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体（代理注入页等）：data 保持 null，按 message 兜底 */
    }
    throw new ApiError(r.status, `session-open ${r.status}`, data);
  }
  return (await r.json()) as SessionOpenResult;
}

// ==== 历史会话区（spec 2026-09-20-mobile-archive-history §6.1）====
export interface ArchivedSession {
  sessionId: string;
  agentType: string;
  projectPath: string;
  projectName: string;
  title: string | null;
  lastStatus: string;
  lastSeenAt: string;
  /** 软归档活会话标记（体验批二）：true = 看板隐藏中的活会话（APP 形态），
   *  详情页动作是「移回看板」而非「在桌面端打开」 */
  hiddenAlive?: boolean;
}

export interface ArchivedPayload {
  archived: ArchivedSession[];
  projects: string[];
}

/** 懒加载归档列表（进入历史页/切换天数时调用；403 → null 回配对页） */
export async function fetchArchivedSessions(days: 1 | 3 | 7): Promise<ArchivedPayload | null> {
  const r = await fetch(`/m/api/v1/sessions-archived?days=${days}`);
  if (r.status === 403) return null;
  if (!r.ok) {
    throw new ApiError(r.status, `sessions-archived ${r.status}`);
  }
  return r.json() as Promise<ArchivedPayload>;
}

/** 归档手动管理（spec 裁决 8）：带 id = 单条移除；缺省 = 清空全部 */
export async function deleteArchivedSession(sessionId?: string): Promise<number> {
  const qs = sessionId ? `?session_id=${encodeURIComponent(sessionId)}` : "?all=1";
  const r = await fetch(`/m/api/v1/sessions-archived${qs}`, { method: "DELETE" });
  if (!r.ok) throw new ApiError(r.status, `sessions-archived DELETE ${r.status}`);
  const data = (await r.json()) as { deleted: number };
  return data.deleted;
}

// ==== 看板关闭/软归档（2026-09-20 体验批二）====

/** {sessionId} 请求体（close/hide/unhide 三端点共用） */
function sessionActionBody(sessionId: string): string {
  return JSON.stringify({ sessionId });
}

/** 远程硬杀 CLI 会话终端（桌面 kill_session 同内核）：进程死 → 3s 内下板 → 进历史归档。
 *  App 形态端点拒绝（400 form_not_supported）——软归档走 hideSession */
export async function closeSession(sessionId: string): Promise<void> {
  const r = await fetch("/m/api/v1/session-close", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: sessionActionBody(sessionId),
  });
  if (!r.ok) throw new ApiError(r.status, `session-close ${r.status}`);
}

/** APP 形态软归档（看板隐藏，不杀进程、可逆）：任意状态可归档（叉不挑颜色），
 *  等同桌面端叉掉——不自动回归，恢复唯一路径 = 历史页「移回看板」（unhideSession）；
 *  CLI 会话端点拒绝（400 form_not_supported）——硬杀走 closeSession */
export async function hideSession(sessionId: string): Promise<void> {
  const r = await fetch("/m/api/v1/session-hide", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: sessionActionBody(sessionId),
  });
  if (!r.ok) throw new ApiError(r.status, `session-hide ${r.status}`);
}

/** 解除软归档（移回看板）。幂等：不在隐藏集也 ok（removed=0） */
export async function unhideSession(sessionId: string): Promise<void> {
  const r = await fetch("/m/api/v1/session-unhide", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: sessionActionBody(sessionId),
  });
  if (!r.ok) throw new ApiError(r.status, `session-unhide ${r.status}`);
}

// ==== Phase C C10：远程新建会话客户端（spec §3/§5；三端点）====

/** 新建会话目标目录候选（GET /create-projects 载荷条目，与 Rust `CreateProjectDto`
 *  camelCase 序列化逐字段对应，勿漂移）：path = 展示形态（后端已剥尾分隔符、滤除
 *  非 ASCII 与 create_path 拒绝码命中项——「不给不能用的候选」）；
 *  lastActiveAt = RFC3339 UTC（秒精度，列表按它降序）；
 *  tools = 该项目近 N 天出现过的工具 id；
 *  activeTools = 其中**当前有活跃会话**的子集（C11 黄字「该项目已有该工具的活跃
 *  会话」判据，≥1 语义，与 pairingAmbiguous 分层） */
export interface CreateProjectView {
  path: string;
  lastActiveAt: string;
  tools: string[];
  activeTools: string[];
}

/** GET /create-projects 载荷 */
export interface CreateProjectsPayload {
  projects: CreateProjectView[];
}

/** 拉取新建会话的目标目录候选（days = 活跃回溯窗，后端 clamp 1–365，缺省 7）。
 *  403 → null（设备失效回配对页，fetchArchivedSessions 同口径）；其余非 2xx /
 *  网络异常 → 抛 ApiError（调用方可静默降级为手填路径，不阻塞表单） */
export async function fetchCreateProjects(days = 7): Promise<CreateProjectsPayload | null> {
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/create-projects?days=${days}`);
  } catch (e) {
    throw new ApiError(null, `create-projects 网络异常: ${String(e)}`);
  }
  if (r.status === 403) return null; // 设备失效 → 回配对页
  if (!r.ok) throw new ApiError(r.status, `create-projects ${r.status}`);
  return r.json() as Promise<CreateProjectsPayload>;
}

/** POST /session-create 成功回执（200）：taskId = 内存任务簿 id（供
 *  /session-create/status 轮询；兔维斯 重启即失效 → 404）；hasActiveSession = 黄字
 *  信号（同工具同目录已有活跃会话——**不拦截**，C11 据此展示提示） */
export interface CreateSessionAccepted {
  taskId: number;
  hasActiveSession: boolean;
}

/** 新建会话的**业务失败**（同步校验链拒绝，非异常，C11 分診文案）：
 *  - bad_request（400）：reasonCode ∈ tool_unavailable / empty / not_absolute /
 *    non_ascii_path / not_local_volume / bad_windows_form / blacklisted /
 *    mkdir_failed / path_too_long（端点层码全集，与 CreateSessionSheet 的
 *    CREATE_REJECT_LABELS 同步维护）；reason 为后端定位详情（黑名单命中段+所属表等，修复批 I1——
 *    展示层 reason 优先于本地码表）；裸 bad_request（入参超长等）无该二键；
 *  - conflict（409）：全局单飞占用（已有非终态创建任务——终态 done/failed
 *    不占额度）。 */
export type CreateSessionError =
  { kind: "bad_request"; reasonCode?: string; reason?: string } | { kind: "conflict" };

/** 发起远程新建会话任务（异步管线：起窗 → 处置弹窗 → 注入首句 → 等物化；管线
 *  detached 推进，不依赖手机持续在线）。200 → 任务已占单飞并立即回执，进度轮询
 *  [`fetchCreateStatus`]；400/409 → **返回类型化结果**（不抛异常）；
 *  其余非 2xx（403 设备失效 / 5xx）与网络异常 → 抛 ApiError（既有 POST 惯例，
 *  错误体 JSON 解析进 data）。 */
export async function createSession(body: {
  tool: string;
  projectPath: string;
  firstMessage?: string;
}): Promise<CreateSessionAccepted | CreateSessionError> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-create", {
      method: "POST",
      headers: { "content-type": "application/json" },
      // firstMessage 缺省时 JSON.stringify 自动省略该键（后端缺省探针 hi）
      body: JSON.stringify(body),
    });
  } catch (e) {
    throw new ApiError(null, `session-create 网络异常: ${String(e)}`);
  }
  if (r.status === 400) {
    // reasonCode 是 400 的分診依据（工具门/路径码/mkdir）——解析失败则缺省，
    // 调用方按通用校验失败文案兜底（fetchFile/sessionApprove 惯例）
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体（代理注入页等）：reasonCode 缺省 */
    }
    const raw = data?.reasonCode;
    const rawReason = data?.reason;
    return {
      kind: "bad_request",
      reasonCode: typeof raw === "string" ? raw : undefined,
      reason: typeof rawReason === "string" ? rawReason : undefined,
    };
  }
  if (r.status === 409) return { kind: "conflict" };
  if (!r.ok) {
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体：data 保持 null，按状态码兜底 */
    }
    throw new ApiError(r.status, `session-create ${r.status}`, data);
  }
  return (await r.json()) as CreateSessionAccepted;
}

// ==== H10（Task 12）：zcode 无头新建 ====

/** 新建候选项目（与 Rust `zcode_create::ProjectCandidate` + info 端点的 JSON 逐字段对应，
 *  勿漂移）：`source` = 来源（`trusted` = APP 信任表主源 / `board` = 看板快照项目）；
 *  `trusted` = 是否在 APP 信任表内——**false 必须如实标注**：该目录的新会话 APP 永不收录，
 *  仅 MAM 可见；`note` = 信任档**逐字文案**（后端 `Visibility::note()` 单点，与回执的
 *  `visibilityNote` 同源——前端**不再自带一份**「已信任/未信任」措辞，复审 Minor 2）。
 *  候选列表已由后端按「存在 ∧ 不在同源敏感黑名单内」过滤，前端不做二次筛选。 */
export interface ZcodeCreateCandidate {
  path: string;
  source: "trusted" | "board";
  trusted: boolean;
  note?: string;
}

/** GET /session-create-zcode-info 载荷：`available=false`（H3 总开关关闭）时**不给候选**
 *  （与 `session-send-info` 关闭态同口径），`reasonCode`/`reason` 是后端单点码与逐字文案；
 *  `warning` = 选中项目的黄字信号（同项目已有在册 zcode 会话，**不拦截**），
 *  `defaultFirstText` = 首句默认值（spec H10 探针 `hi`，后端单点下发）。 */
export interface ZcodeCreateInfo {
  available: boolean;
  reasonCode?: string;
  reason?: string;
  tool?: string;
  defaultFirstText?: string;
  candidates?: ZcodeCreateCandidate[];
  warning?: string;
}

/** 新会话确认来源的**跨语言唯一名单**（与 `tests/fixtures/zcode_create_confirmations.json`
 *  逐项相等——本常量、Rust 侧 `remote::api::confirmation_wire`、夹具三处任一漂移即有一侧先红，
 *  做法同 `headless_stages.json`；复审 Minor 4）。语义见 [`ZcodeCreateConfirmation`]。 */
export const ZCODE_CREATE_CONFIRMATIONS = ["stdout_frame", "store", "none"] as const;

/** 新会话确认来源（取自 [`ZCODE_CREATE_CONFIRMATIONS`]，与 Rust
 *  `zcode_create::NewSessionConfirmation` 的 wire 词对应）：
 *  `stdout_frame` = CLI 回执帧点名；`store` = 会话库发现（创建前不在册、且建行时刻不早于
 *  回合起点的新会话）；`none` = **未确认**——此时 `sessionId` 恒空串（后端绝不编造 sess_id，
 *  前端也不得凭空显示）。 */
export type ZcodeCreateConfirmation = (typeof ZCODE_CREATE_CONFIRMATIONS)[number];

/** POST /session-create-zcode 回执（HTTP 恒 200，语义在 body——与无头封套同口径）：
 *  - `sessionId` 只在**确认到**新会话时非空；
 *  - `receipt` = Task 6 回执原样透出（status/stage/reason 的分诊复用 SessionDetail 单点）；
 *  - `visibility`/`visibilityNote` 只在确认到新会话时下发（未确认时什么都没落到工作区，
 *    承诺「重启后可见」就是谎报）；
 *  - `warning` = 黄字信号（不拦截）。 */
export interface ZcodeCreateResult {
  channel?: string;
  sessionId: string;
  confirmation: ZcodeCreateConfirmation;
  receipt: HeadlessReceipt;
  visibility?: SendInfo["visibility"];
  visibilityNote?: string;
  warning?: string | null;
}

/** 拉取新建表单前置面（挂载时一次；可选点名项目以取黄字信号）。403（设备失效，与
 *  fetchSendInfo 同语义）→ null；其余非 2xx / 网络异常 → 抛 ApiError，调用方如实提示
 *  「表单不可用」，不渲染半截表单。 */
export async function fetchZcodeCreateInfo(project?: string): Promise<ZcodeCreateInfo | null> {
  const qs = project ? `?project=${encodeURIComponent(project)}` : "";
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-create-zcode-info${qs}`);
  } catch (e) {
    throw new ApiError(null, `session-create-zcode-info 网络异常: ${String(e)}`);
  }
  if (r.status === 403) return null; // 设备失效 → 回配对页（fetchSendInfo 同口径）
  if (!r.ok) throw new ApiError(r.status, `session-create-zcode-info ${r.status}`);
  return (await r.json()) as ZcodeCreateInfo;
}

/** 无头新建一个 zcode 会话并注入首句（H10）。`firstText` 缺省时**不带上该键**
 *  （后端按 spec H10 默认探针 `hi` 补齐——默认值单点在服务端）。非 2xx（400 请求不合法 /
 *  403 总开关关闭或设备失效）→ 抛 ApiError（403 的 `reason` 是后端逐字文案，解析进 data
 *  供调用方展示——对齐 sessionSend 惯例）。 */
export async function sessionCreateZcode(
  project: string,
  firstText?: string
): Promise<ZcodeCreateResult> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/session-create-zcode", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ project, ...(firstText ? { firstText } : {}) }),
    });
  } catch (e) {
    throw new ApiError(null, `session-create-zcode 网络异常: ${String(e)}`);
  }
  if (!r.ok) {
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体（代理页等）：data 保持 null，按状态码兜底 */
    }
    throw new ApiError(r.status, `session-create-zcode ${r.status}`, data);
  }
  return (await r.json()) as ZcodeCreateResult;
}

/** 新建任务快照（GET /session-create/status 载荷，与 Rust `CreateTaskShared`
 *  camelCase 序列化逐字段对应，勿漂移）：
 *  phase ∈ opening_terminal / dialog_handling / injecting_first /
 *  waiting_materialize / done / failed（终态 done|failed 释放单飞额度）；
 *  detail = 中文现场/提示（失败原因，或成功终态附带的 codex hooks 信任提示；
 *  缺省 null）；sessionId 仅成功终态有值；spawnedPid = 起窗后锚定的 TUI pid
 *  （锚定前 null）。 */
export interface CreateStatusPayload {
  phase: string;
  detail: string | null;
  sessionId: string | null;
  spawnedPid: number | null;
}

/** 任务失效（404 no_task）：兔维斯 重启丢内存任务簿，或 taskId 非法——C11 文案
 *  「任务已失效（主机可能重启），请重试」 */
export type CreateStatusError = { kind: "no_task" };

/** 轮询新建任务进度（2s 节奏由 C11 页面层驱动，本层无状态——
 *  fetchSessionMessages 同纪律）。404 → {kind:"no_task"}（类型化结果，不抛）；
 *  其余非 2xx / 网络异常 → 抛 ApiError。 */
export async function fetchCreateStatus(
  taskId: number
): Promise<CreateStatusPayload | CreateStatusError> {
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-create/status?taskId=${taskId}`);
  } catch (e) {
    throw new ApiError(null, `session-create/status 网络异常: ${String(e)}`);
  }
  if (r.status === 404) return { kind: "no_task" };
  if (!r.ok) throw new ApiError(r.status, `session-create/status ${r.status}`);
  return (await r.json()) as CreateStatusPayload;
}

// ==== 2026-10-08 子 agent 运行 chip（spec 2026-10-08-mobile-subagent-chips §5.2/§6）====

/** GET /session-subagents 载荷单条（与 Rust monitor::subagents::SubagentView 的
 *  camelCase 序列化逐字段对应，勿漂移）。spawnTs=null：首条时间戳未落盘（spawn
 *  竞态，下轮自愈）——前端不显示时长只显 token。
 *  2026-10-09 观察台：**全量名单**（含终态）——status=running → chip 活跃区；
 *  idle → 清单卡灰点冻结（endTs = 冻结锚，elapsed = (endTs ?? now) − spawnTs）。 */
export interface SubagentView {
  id: string;
  name: string;
  description: string | null;
  spawnTs: string | null;
  tokens: { input: number; cacheRead: number; cacheCreation: number; output: number };
  status: "running" | "idle";
  endTs: string | null;
}

/** 拉取会话的子 agent 全量名单（含终态；运行过滤在 chip 层）。空列表 = 该会话
 *  无子 agent——端点是空态唯一权威（前端不另特判）。非 2xx / 网络异常 → 抛
 *  ApiError（调用方静默，ModeBar 同惯例） */
export async function fetchSessionSubagents(
  agentType: string,
  sessionId: string
): Promise<SubagentView[]> {
  const q = new URLSearchParams({ agent_type: agentType, session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-subagents?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-subagents 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-subagents ${r.status}`);
  const data = (await r.json()) as { subagents: SubagentView[] };
  return data.subagents;
}

// ==== 2026-10-09 观察台 §三：子 agent 详情（/session-subagent-messages）====

/** 详情载荷：messages/truncated 与 /session-messages 同形（渲染器零适配）；
 *  supported=false = 该工具暂不支持查看详情（opencode/kimi/codex——spec §三.6，
 *  如实申报不空白页不假数据），前端显示明确提示态 */
export interface SubagentMessagesPage {
  messages: SessionMessage[];
  truncated: boolean;
  supported: boolean;
}

/** 拉取子 agent 执行过程（claude）。404（会话/转写不存在）与非 2xx → ApiError；
 *  supported=false 是 200 正常载荷不是错误 */
export async function fetchSubagentMessages(
  agentType: string,
  sessionId: string,
  subagentId: string,
  limit = 200
): Promise<SubagentMessagesPage> {
  const q = new URLSearchParams({
    agent_type: agentType,
    session_id: sessionId,
    subagent_id: subagentId,
    limit: String(limit),
  });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-subagent-messages?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-subagent-messages 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-subagent-messages ${r.status}`);
  return (await r.json()) as SubagentMessagesPage;
}

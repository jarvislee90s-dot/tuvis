// ZCode 统一控制状态 hook（T7.2 Slice A2a）：手机控制面的唯一读/写接驳层。
//
// 纪律（与计划 T7.2 / shared-context 对齐）：
// - 连接态**只**来自 `GET /zcode/connection`：已保存链接、可读会话、成功 GET 都
//   不代表已连接；确认前 `connection=null`，绝不虚构 `paired`。
// - `submit` 只走官方 `POST /zcode/actions`（一次提交一个 actionId；重试 = 新提交 =
//   新 ID）。actionId 用 `newActionId()`（codexIpcApi 的 LAN 安全兜底）生成——手机页
//   由明文 TCP 伺服（http://<LAN-IP>:<port>/m），非安全上下文里 `crypto.randomUUID`
//   不存在，直调会让全部动作静默失败。**不**回退旧 `session-send`、无头 CLI 或
//   终端按键；unknown 绝不由本 hook 自动重发。
// - 回执状态如实透传：`accepted` 只是宿主已受理（≠ 已生效）；`confirmed` 才是
//   已生效。只有 `send` 的 `accepted` 允许清本次草稿；`send_failed`/`rejected`/
//   `unknown` 一律保留草稿。网络/非 2xx 不伪造回执（抛出，`reason` 已置码）。
// - `refresh` 是 SSE 的纠偏入口：lag/断流/换代只重拉 GET（连接 + 会话），不置
//   loading，不清已有视图；迟到的旧会话/旧代次事件一律作废。
// - SSE（`/zcode/events?sessionId=`）：`snapshot`/`connection`/`session` 帧直接落地
//   DTO；`action` 只作纠偏信号（重拉 GET，**绝不重放动作**）；`resync`（lag）同样
//   只重拉 GET。`session`/`snapshot` 帧带回 `ready=false`（投影已被 Hub 显式失效）
//   且落地成功时，同一代次**补一次** GET 重新绑定/现读（重复同代不刷新，防循环）。
//   失效后**首次** `ready=true` 只代表 Hub 投影恢复，能力矩阵可能仍沿用降级时的全关
//   （坏帧降级后 Hub 事件就是这么发的）——必须再补一次权威 GET 由 `Hub::session()`
//   现算能力（每失效周期至多一次，绝不循环）。守卫只在 GET 落地 `ready=true`、切会话、
//   连接换代时清空。卸载/切会话关闭流；别的会话或旧代次帧忽略。
// - 阅读链（看板/正文/文件预览）不依赖本 hook 成功。
// - 落地分三种语义（F1，见 [`resolveSessionViewLanding`]）：不比当前旧的视图**完整
//   落地**；同会话同代但**更旧**的 `ready=false` 只做**安全降级**（内容留在较新视图，
//   只关写面）；`recovery` 且更旧只做**能力收窄**（不得开机能力、不得替换内容）。
//   迟到的旧 GET 绝不能让队列/pending 整体回退（§3.1 / §13 反模式 6）。
import { useCallback, useEffect, useRef, useState } from "react";
import {
  ZcodeControlApiError,
  getZcodeConnection,
  getZcodeSession,
  postZcodeAction,
  zcodeEventsUrl,
  type ActionReceipt,
  type CapabilityFlag,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type SessionView,
} from "@/lib/api/zcodeControl";
import { newActionId } from "./codexIpcApi";

export interface UseZcodeControlResult {
  /** 服务端连接视图；null = 尚未确认（也是唯一「未知」表达，不猜 paired）。 */
  connection: ConnectionView | null;
  /** 会话投影（pending/control/selection/capabilities）；null = 未确认/读失败。 */
  session: SessionView | null;
  /**
   * 当前就地原因（冻结错误码，不是用户文案；UI 按码取 zh/en）：
   * 连接未就绪时取连接态、提交失败时取回执 reasonCode；accepted/confirmed/noop
   * 与正常连接为 null。
   */
  reason: string | null;
  /** 首拍/切会话的 GET 在途；`refresh` 是静默纠偏，不置本态。 */
  loading: boolean;
  /** 动作 POST 在途（并发提交计数归零才复位）。 */
  submitting: boolean;
  /**
   * 最近一次 **send** 提交是否已受理、本次草稿可清：
   * 仅 `accepted` 置 true；`send_failed`/`rejected`/`unknown` 与网络失败保持
   * false（草稿必须保留）；非 send 动作不改动本标记。
   */
  draftCleared: boolean;
  /** 提交一次统一控制动作；服务端回执原样返回，传输层失败抛 ZcodeControlApiError。 */
  submit(action: ControlAction): Promise<ActionReceipt>;
  /** 重拉连接 + 会话（SSE 重连/换代后的 GET 纠偏；不重放动作）。 */
  refresh(): Promise<void>;
}

/** 连接态 → 就地原因码：paired/connecting 无错误；其余优先服务端 reasonCode，
 *  缺省用状态原值（needs_config/needs_auth/offline/conflict/unknown 均在冻结码内）。 */
function connectionReason(connection: ConnectionView): string | null {
  if (connection.state === "paired" || connection.state === "connecting") return null;
  return connection.reasonCode ?? connection.state;
}

/** 传输/HTTP 异常 → 冻结原因码：网络层（status=null）按 offline，其余未知。 */
function errorReason(error: unknown): string {
  if (error instanceof ZcodeControlApiError) {
    if (error.code !== null) return error.code;
    return error.status === null ? "offline" : "unknown";
  }
  return "unknown";
}

/** 回执 → 就地原因码：accepted/confirmed/noop 无错误；失败态用服务端码，
 *  缺省按 unknown（绝不把失败态洗成成功文案）。 */
function receiptReason(receipt: ActionReceipt): string | null {
  if (receipt.state === "accepted" || receipt.state === "confirmed" || receipt.state === "noop") {
    return null;
  }
  return receipt.reasonCode ?? "unknown";
}

/** 帧形状守卫：坏帧/残缺 DTO 一律跳过（不因单帧异常断流或写入假视图）。 */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isConnectionView(value: unknown): value is ConnectionView {
  return (
    isRecord(value) && typeof value.state === "string" && Number.isSafeInteger(value.generation)
  );
}

function isSessionView(value: unknown): value is SessionView {
  return (
    isRecord(value) &&
    typeof value.sessionId === "string" &&
    Number.isSafeInteger(value.generation) &&
    typeof value.ready === "boolean"
  );
}

/**
 * 会话视图来源：`safety-degrade` = 已确认的**安全降级**（`ready=false` 的实时失效）。
 *
 * 业务 revision 只能挡「同代旧快照」，绝不能挡安全降级：掉线/坏帧/投影失效时即使
 * 没有可比较的 revision，也必须让写面立刻关闭（安全先行）。内容恢复仍走既有
 * `staleRefreshRef`/`recoveryRefreshRef` 预算，不在本函数里重写恢复逻辑。
 */
export type SessionOrigin = "sse" | "get" | "safety-degrade" | "recovery";

/**
 * 一份 `incoming` 视图的**落地方式**（F1：把「落地」拆成三种语义）：
 *
 * - `replace`：完整落地（`setSession`）。只在 `incoming` **不比当前旧**时允许：
 *   换代更大 / 同代 revision 不更小 / 当前无锚（含切会话）。
 * - `degrade`：**仅安全降级**。`incoming` 同会话同代但更旧、且 `ready=false`：
 *   内容字段（pending/queue/control/selection/revision）一律留在较新视图，只把
 *   `ready` 置 false、能力矩阵全关。迟到的旧 GET 绝不能把队列/pending 倒回旧快照。
 * - `caps`：**仅能力恢复**。`origin=recovery` 且 `incoming` 更旧：只允许把能力由开
 *   改关（更严），不得开机能力、不得替换内容字段。
 * - `drop`：什么都不写（旧代次 / 别的会话 / 更旧的普通 `ready=true` 快照）。
 */
export type SessionApplyMode = "replace" | "degrade" | "caps" | "drop";

/** 已落地会话视图的时序锚（只含比较所需的四个字段）。 */
export interface AppliedSessionView {
  sessionId: string;
  generation: number;
  revision: number | null;
  ready: boolean;
}

/** 同会话同代：`incoming` 的 revision 是否**严格更旧**（纯函数）。 */
function isOlderRevision(current: AppliedSessionView, incoming: AppliedSessionView): boolean {
  // 当前无锚（revision=null）：没有任何可比较序号，一律不判旧。
  if (current.revision === null) return false;
  // 宿主没给序号 ≠ 比已知序号新（与 shouldApplySessionView 既有口径一致）。
  if (incoming.revision === null) return true;
  return incoming.revision < current.revision;
}

/**
 * 同代 revision 守卫（纯函数）：`incoming` 该按哪种语义落地。
 *
 * 规则（F1）：
 * - 跨会话 / 换代更大 / 当前无锚 → `replace`（完整落地）；
 * - 旧代次 → `drop`；
 * - 同会话同代、`incoming` 不比当前旧（revision 不更小，或当前无锚）→ `replace`；
 * - 同会话同代、`incoming` 更旧：
 *   - `ready=false` → `degrade`：**安全降级独立放行**（实时失效必须能关写面），
 *     但只关写面，内容留在较新视图——旧 GET 不得恢复旧队列/pending；
 *   - `origin=recovery` 且 `ready=true` → `caps`：只收窄能力矩阵（把开改关），
 *     绝不复活旧内容；
 *   - 其余（普通 GET/SSE 的更旧 `ready=true` 快照）→ `drop`：旧内容不得回归。
 */
export function resolveSessionViewLanding(
  current: AppliedSessionView | null,
  incoming: AppliedSessionView,
  origin: SessionOrigin
): SessionApplyMode {
  if (current === null) return "replace";
  if (current.sessionId !== incoming.sessionId) return "replace";
  if (incoming.generation > current.generation) return "replace";
  if (incoming.generation < current.generation) return "drop";
  if (!isOlderRevision(current, incoming)) return "replace";
  if (!incoming.ready) return "degrade";
  if (origin === "recovery") return "caps";
  return "drop";
}

/**
 * 同代 revision 守卫的布尔外壳（保留给调用方/测试）：`incoming` 是否**有任何**写入。
 *
 * 语义收窄为「不整份落地」不再等于「丢弃」：安全降级走 `degrade`、恢复走 `caps`
 * 也算落地（都会写 state），但都不允许覆盖较新的业务内容。
 */
export function shouldApplySessionView(
  current: AppliedSessionView | null,
  incoming: AppliedSessionView,
  origin: SessionOrigin
): boolean {
  return resolveSessionViewLanding(current, incoming, origin) !== "drop";
}

/** 降级能力矩阵的冻结原因码（与 Rust `CapabilityView::default()` 逐字一致）。 */
const DEGRADED_REASON_CODE = "needs_evidence";

function closedFlag(): CapabilityFlag {
  return { enabled: false, reasonCode: DEGRADED_REASON_CODE };
}

/**
 * 已降级视图的写能力矩阵：八项全关并带冻结原因码。
 *
 * 唯一一处在手机侧构造「全关」矩阵——安全降级不得携带旧的开闸能力，也不许另造
 * 第二套字段/开关（与 Rust `CapabilityView::default()` 同形）。
 */
export function degradedCapabilities(): CapabilityView {
  return {
    send: closedFlag(),
    singleSelect: closedFlag(),
    multiSelect: closedFlag(),
    freeText: closedFlag(),
    interrupt: closedFlag(),
    approvalReply: closedFlag(),
    model: closedFlag(),
    thoughtLevel: closedFlag(),
  };
}

const CAPABILITY_KEYS = [
  "send",
  "singleSelect",
  "multiSelect",
  "freeText",
  "interrupt",
  "approvalReply",
  "model",
  "thoughtLevel",
] as const;

/** 逐项取「更严」：只在两侧都开时才算开；关闭时保留关闭侧的原因码。 */
function tightenCapabilities(current: CapabilityView, incoming: CapabilityView): CapabilityView {
  const merged = {} as CapabilityView;
  for (const key of CAPABILITY_KEYS) {
    const cur = current[key];
    const next = incoming[key];
    const enabled = cur.enabled && next.enabled;
    merged[key] = {
      enabled,
      reasonCode: enabled ? null : cur.enabled ? next.reasonCode : cur.reasonCode,
    };
  }
  return merged;
}

/**
 * 仅安全降级（纯函数）：内容留在 `current`，只关写面。
 *
 * `incoming` 是**更旧**的降级快照（迟到的旧 GET / 旧帧）。它的 pending/queue/
 * control/selection 一律不采信——那正是「旧队列复活」的来源；只借它的
 * `ready=false` 关写面，能力矩阵回全关。`incoming.ready=true`（异常路径）时保留
 * `current.ready`，绝不由旧视图开写面。
 */
export function mergeDegradedView(current: SessionView, incoming: SessionView): SessionView {
  return {
    ...current,
    ready: incoming.ready ? current.ready : false,
    capabilities: degradedCapabilities(),
  };
}

/**
 * 仅能力恢复（纯函数）：内容留在 `current`，能力矩阵逐项取「更严」。
 *
 * 用于 `origin=recovery` 且 `incoming` 更旧的权威现读：它可以把能力由开改关
 * （更严），但**不得**开机能力、不得替换内容字段——否则旧视图会复活旧队列项。
 */
export function mergeRecoveredCaps(current: SessionView, incoming: SessionView): SessionView {
  return {
    ...current,
    capabilities: tightenCapabilities(current.capabilities, incoming.capabilities),
  };
}

/** `ControlEvent` 的线上形状（`snapshot`/`resync` 帧无此包裹，单独处理）。 */
interface ControlEventFrame {
  seq: number;
  generation: number;
  sessionId: string | null;
  kind: string;
  data: unknown;
}

/** 断流重连的指数退避：1s → 2s → 4s → 8s → 16s → 30s（封顶后维持 30s）。
 *  每次断流只做**一次**纠偏 GET（连接 + 会话各一发），随后按本退避等待重连；
 *  退避在「流恢复」时复位——任一帧落地即算恢复（服务端连上必先发 snapshot）。 */
const SSE_RECONNECT_BASE_MS = 1000;
const SSE_RECONNECT_MAX_MS = 30_000;

/** 每会话代次的恢复 GET 尝试上限（首次 + 一次重试）：失败的恢复不得无限重试，
 *  也不得被单次失败毒化——每次 ready=false 清恢复标记，但尝试次数封顶。 */
const MAX_RECOVERY_ATTEMPTS_PER_GENERATION = 2;

export default function useZcodeControl(sessionId: string): UseZcodeControlResult {
  const [connection, setConnection] = useState<ConnectionView | null>(null);
  const [session, setSession] = useState<SessionView | null>(null);
  const [reason, setReason] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [submitting, setSubmitting] = useState(false);
  const [draftCleared, setDraftCleared] = useState(false);

  /** 当前会话哨兵：null = 已卸载；迟到的旧会话响应不得落地。 */
  const sessionRef = useRef<string | null>(sessionId);
  /** GET 轮次：切会话/refresh 开启新一轮，旧轮迟到响应一律作废（最新一轮胜出）。 */
  const readRunRef = useRef(0);
  /** 在途提交计数：并发提交时最后一个结束才复位 submitting。 */
  const inflightSubmitsRef = useRef(0);
  /** 已落地的代次（连接/会话各自）：SSE/GET 帧低于它 = 迟到旧代，丢弃。 */
  const generationRef = useRef({ connection: -1, session: -1 });
  /**
   * 最近一次**成功落地**的会话视图时序锚（会话 + 代次 + revision + ready）：
   * 同代 revision 守卫据此丢弃慢 GET 的旧快照。切会话/连接换代时复位。
   */
  const appliedSessionRef = useRef<AppliedSessionView | null>(null);
  /**
   * 最近一次**成功落地**的连接视图（state + generation）：判「同代状态跃迁」用。
   * 只在 applyConnection 真正写入 state 时更新——同代 `needs_auth/connecting → paired`
   * 也必须补拉会话，仅靠代次会漏掉（代次没变的配对完成是合法场景）。
   */
  const appliedConnectionRef = useRef<ConnectionView | null>(null);
  /**
   * 已因 `ready=false` 补拉过 GET 的会话代次（null = 无待自愈）。
   *
   * Hub 在 Data 溢出/投影失效后把**每个**跟踪会话降级为 `ready=false` 并逐条广播
   * Session 帧；手机必须自己重拉一次 GET 重新绑定/现读，否则页面永远停在失效投影。
   * 同一代次只允许一发（重复降级帧/重连快照不得打刷新风暴）。守卫**只在 GET 落地**
   * `ready=true`（见 applyRead）或切会话/连接换代时清空：SSE 帧的 `ready=true` 只是
   * Hub 投影恢复，能力矩阵可能仍是降级后的全关，不能据此清守卫。GET 直接读到
   * `ready=false`（页面在失效窗口内打开、无降级帧）也会记为「本代次已失效」。失败的
   * refresh 不清守卫：服务端仍返回失效投影时重试只会变成循环。
   */
  const staleRefreshRef = useRef<number | null>(null);
  /**
   * 已为「失效后首次 `ready=true`」补拉过**权威** GET 的会话代次（null = 无恢复请求）。
   *
   * 坏帧/投影失效后 Hub 的能力矩阵被清成基线全关；恢复帧的 `ready=true` 事件可能
   * 沿用这份全关矩阵（Hub `apply_relay_session` 复用 last_session），手机若直接
   * 采信就会让发送按钮持续灰着。恢复 GET 由 `Hub::session()` 现算权威能力（含验收
   * 许可）。每代次至多 [`MAX_RECOVERY_ATTEMPTS_PER_GENERATION`] 次（首次 + 一次
   * 重试）；封顶后不再自动重试，换代/切会话/GET 落地 ready=true 重置。绝不循环、
   * 绝不需要手动 refresh。
   */
  const recoveryRefreshRef = useRef<number | null>(null);
  /**
   * 本会话代次已发出的恢复 GET 次数（封顶
   * [`MAX_RECOVERY_ATTEMPTS_PER_GENERATION`]）。GET 落地 ready=true、切会话、连接
   * 换代时重置；失败的恢复只消耗额度，不会把本代次永久毒化。
   */
  const recoveryAttemptsRef = useRef<{ generation: number; attempts: number }>({
    generation: -1,
    attempts: 0,
  });

  /** 落地连接视图并推进代次；返回 false = 旧代次（不写 state）。 */
  const applyConnection = useCallback((view: ConnectionView): boolean => {
    if (view.generation < generationRef.current.connection) return false;
    generationRef.current.connection = view.generation;
    appliedConnectionRef.current = view;
    setConnection(view);
    return true;
  }, []);

  /**
   * 落地会话投影并推进代次；别的会话/旧代次/不允许写入 → false（不写 state）。
   *
   * 三种落地语义（见 [`resolveSessionViewLanding`]）：完整落地；**仅安全降级**
   * （保持较新的 pending/queue/control/selection，只关写面）；**仅能力恢复**
   * （只收窄能力矩阵）。迟到的旧 GET 绝不能整份覆盖较新的业务内容。
   */
  const applySessionView = useCallback((view: SessionView, origin: SessionOrigin): boolean => {
    if (view.sessionId !== sessionRef.current) return false;
    // 旧代次仍然直接丢弃（既有规则）；同代单调由纯函数统一判定（含安全降级分流）。
    if (view.generation < generationRef.current.session) return false;
    const current = appliedSessionRef.current;
    const incoming: AppliedSessionView = {
      sessionId: view.sessionId,
      generation: view.generation,
      revision: view.revision,
      ready: view.ready,
    };
    const landing = resolveSessionViewLanding(current, incoming, origin);
    if (landing === "drop") return false;
    if (landing === "degrade") {
      // 只关写面：内容留在较新视图（绝不 setSession(view) 整份覆盖）。
      setSession((prev) => (prev === null ? view : mergeDegradedView(prev, view)));
      appliedSessionRef.current = {
        sessionId: view.sessionId,
        generation: view.generation,
        revision: current?.revision ?? view.revision,
        ready: false,
      };
      return true;
    }
    if (landing === "caps") {
      // 只收窄能力：内容与 ready 一律留在较新视图，并保持时序锚不变。
      setSession((prev) => (prev === null ? view : mergeRecoveredCaps(prev, view)));
      return true;
    }
    generationRef.current.session = view.generation;
    appliedSessionRef.current = incoming;
    setSession(view);
    // 守卫**不**在这里按 `ready` 清空：SSE 帧的 ready=true 可能仍带着降级后的全关
    // 能力矩阵，只有 GET 落地 ready=true（applyRead）才算权威恢复。
    return true;
  }, []);

  /** 读连接 + 会话并落地；返回是否真正生效（旧会话/旧轮次 → false，不写 state）。 */
  const applyRead = useCallback(
    async (sid: string, run: number, origin: SessionOrigin = "get"): Promise<boolean> => {
      const isCurrent = () => sessionRef.current === sid && readRunRef.current === run;
      // 两面分开读：任一失败不拖累另一面（控制失败时阅读链照常）。
      const connectionRead = getZcodeConnection().then(
        (value) => ({ ok: true as const, value }),
        (error: unknown) => ({ ok: false as const, error })
      );
      const sessionRead = getZcodeSession(sid).then(
        (value) => ({ ok: true as const, value }),
        (error: unknown) => ({ ok: false as const, error })
      );
      // 各自先落地：连接 GET 挂起时，会话投影仍可先显示（不被慢面拖住）。
      void connectionRead.then((outcome) => {
        if (outcome.ok && isCurrent()) applyConnection(outcome.value);
      });
      void sessionRead.then((outcome) => {
        if (!outcome.ok || !isCurrent()) return;
        // GET 的 ready=false 是真实失效（页面可能在失效窗口内打开）：按安全降级独立
        // 放行（只关写面；内容若比当前旧则保持不倒退），不得被 revision 守卫挡在门外。
        if (!applySessionView(outcome.value, outcome.value.ready ? origin : "safety-degrade"))
          return;
        // 只有 **GET 落地** 的 ready=true 才算权威恢复（能力矩阵已现算）：清掉失效/
        // 恢复守卫。SSE 帧的 ready=true 走 requestStaleRefresh，不在此清。
        if (outcome.value.ready) {
          staleRefreshRef.current = null;
          recoveryRefreshRef.current = null;
          recoveryAttemptsRef.current = { generation: -1, attempts: 0 };
        } else {
          // 页面可能在失效窗口内打开：GET 直接读到 ready=false（没有任何降级 SSE 帧）
          // 也必须记为「本代次已失效」，否则随后的 ready=true（能力全关）不会触发
          // 恢复 GET。恢复尝试计数保留（同代次封顶语义）。
          staleRefreshRef.current = outcome.value.generation;
          recoveryRefreshRef.current = null;
        }
      });
      const [connectionResult, sessionResult] = await Promise.all([connectionRead, sessionRead]);
      if (!isCurrent()) return false;
      // 迟到旧代次 GET 不得改写原因面（I2）：视图代次守卫只挡 state 写入，原因若照旧
      // 求值，会出现「连接已 paired、发送可用，顶部却写未授权」的自相矛盾。旧代次
      // 响应只算读到过，不改 reason；新代次视图已由 SSE/另一次 GET 落地。
      const staleConnection =
        connectionResult.ok && connectionResult.value.generation < generationRef.current.connection;
      const staleSession =
        sessionResult.ok && sessionResult.value.generation < generationRef.current.session;
      if (staleConnection || staleSession) return true;
      // 原因面等两面都到齐再定：连接错误优先，连接正常时取会话错误。
      let nextReason = connectionResult.ok
        ? connectionReason(connectionResult.value)
        : errorReason(connectionResult.error);
      if (!sessionResult.ok && nextReason === null) {
        nextReason = errorReason(sessionResult.error);
      }
      setReason(nextReason);
      return true;
    },
    [applyConnection, applySessionView]
  );

  useEffect(() => {
    let alive = true;
    sessionRef.current = sessionId;
    // 换会话：旧代次与旧「已落地连接视图」一律作废（新会话从自己的首个快照重新起算）。
    generationRef.current = { connection: -1, session: -1 };
    appliedConnectionRef.current = null;
    appliedSessionRef.current = null;
    // 旧会话的失效/恢复守卫不得吞掉新会话（同代次）的第一次降级自愈/恢复补拉。
    staleRefreshRef.current = null;
    recoveryRefreshRef.current = null;
    recoveryAttemptsRef.current = { generation: -1, attempts: 0 };
    const run = ++readRunRef.current;
    // 换会话先清旧视图：旧连接/旧原因/旧草稿标记不得跨会话显示。
    setConnection(null);
    setSession(null);
    setReason(null);
    setDraftCleared(false);
    setLoading(true);
    void applyRead(sessionId, run).then((applied) => {
      if (alive && applied) setLoading(false);
    });
    return () => {
      alive = false;
      sessionRef.current = null;
    };
  }, [sessionId, applyRead]);

  /** 重读连接 + 会话；`recovery=true` 仅由失效/恢复守卫使用（事后权威现读）。 */
  const readSessions = useCallback(
    async (recovery: boolean) => {
      const sid = sessionRef.current;
      if (sid === null) return; // 已卸载：静默
      const applied = await applyRead(sid, ++readRunRef.current, recovery ? "recovery" : "get");
      if (applied) setLoading(false);
    },
    [applyRead]
  );

  /** 公开纠偏入口（SSE 重连/换代）：普通 GET，受同代 revision 单调约束。 */
  const refresh = useCallback(async () => {
    await readSessions(false);
  }, [readSessions]);

  /**
   * 失效/恢复的一次性自愈（stale 每代次最多一发，恢复最多
   * [`MAX_RECOVERY_ATTEMPTS_PER_GENERATION`] 次，绝不循环）：
   *
   * - `ready=false`：每次先清 recoveryRef（失败重试不被毒化）；同代已请求 → 吞掉，
   *   否则记 staleRef、补一发 GET；
   * - `ready=true` 且 recoveryRef 已是本代 → 恢复 GET 已请求/在途，吞掉（有界）；
   * - `ready=true` 且 staleRef 是本代 → **失效后的首次恢复**：记 recoveryRef 并补一发
   *   权威 GET（Hub 事件此时的能力矩阵可能是降级后的全关，必须由 GET 现算）；每代次
   *   至多 `MAX_RECOVERY_ATTEMPTS_PER_GENERATION` 次（首次 + 一次重试），封顶后 no-op；
   * - 其余（无失效周期）→ 清空两个守卫。
   *
   * 守卫只在 GET 落地 `ready=true`（applyRead）、切会话、连接换代时清空。
   */
  const requestStaleRefresh = useCallback(
    (view: SessionView) => {
      const generation = view.generation;
      if (!view.ready) {
        // 每次观察到失效都清恢复标记：两次 GET 都失败后，本代次的后续 ready=true
        // 仍可再尝试（额度见 MAX_RECOVERY_ATTEMPTS_PER_GENERATION），不被毒化。
        recoveryRefreshRef.current = null;
        if (staleRefreshRef.current === generation) return;
        staleRefreshRef.current = generation;
        void readSessions(true);
        return;
      }
      if (recoveryRefreshRef.current === generation) return;
      if (staleRefreshRef.current === generation) {
        const attempts = recoveryAttemptsRef.current;
        const used = attempts.generation === generation ? attempts.attempts : 0;
        if (used >= MAX_RECOVERY_ATTEMPTS_PER_GENERATION) return;
        recoveryAttemptsRef.current = { generation, attempts: used + 1 };
        recoveryRefreshRef.current = generation;
        void readSessions(true);
        return;
      }
      staleRefreshRef.current = null;
      recoveryRefreshRef.current = null;
    },
    [readSessions]
  );

  // ==== SSE：会话作用域事件流（DTO 直接落地；lag/断流只重 GET）====
  useEffect(() => {
    if (typeof EventSource === "undefined") return; // 无 SSE 运行时：GET 首拍仍权威
    let stopped = false;
    let source: EventSource | null = null;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    /** 下一次断流后的等待毫秒数（指数退避；流恢复即复位）。 */
    let retryDelay = SSE_RECONNECT_BASE_MS;

    /** 解析并落地一帧；坏帧/残缺 DTO/旧代次/别的会话一律跳过（不断流）。 */
    const dispatch = (name: string, event: Event) => {
      let frame: unknown;
      try {
        frame = JSON.parse((event as MessageEvent).data);
      } catch {
        return;
      }
      if (!isRecord(frame)) return;

      if (name === "snapshot") {
        // 首帧/重连基线：连接与（可绑定时的）会话投影一起纠偏
        const connectionView = isConnectionView(frame.connection) ? frame.connection : null;
        const sessionFrame = isSessionView(frame.session) ? frame.session : null;
        const sessionApplied =
          sessionFrame !== null
            ? applySessionView(sessionFrame, sessionFrame.ready ? "sse" : "safety-degrade")
            : false;
        const connectionApplied = connectionView !== null ? applyConnection(connectionView) : false;
        // 基线里的会话投影已失效（lag 后 ready=false）：与 session 帧同规，一次性自愈。
        if (sessionApplied && sessionFrame !== null) requestStaleRefresh(sessionFrame);
        // 基线说已配对，却没带本会话的有效会话投影（缺失/别的会话/旧代次）：补一次
        // GET 把会话拉齐——首拍会话读失败后仅靠 connection 帧仍可能留下空投影。
        if (connectionApplied && connectionView?.state === "paired" && !sessionApplied) {
          void refresh();
        }
        return;
      }
      if (name === "resync") {
        // lag = 中间事件已丢：只重 GET 自愈，**绝不重放动作**
        void refresh();
        return;
      }

      const envelope = frame as unknown as ControlEventFrame;
      if (!Number.isSafeInteger(envelope.generation)) return;
      const foreign =
        envelope.sessionId !== null &&
        envelope.sessionId !== undefined &&
        envelope.sessionId !== sessionId;
      if (foreign) return;

      if (name === "connection") {
        if (!isConnectionView(envelope.data)) return;
        // 判据 = 上次成功落地的连接视图（state + generation），不是只比代次：
        // 同代 needs_auth/connecting → paired 也是必须补拉会话的合法跃迁。
        const previous = appliedConnectionRef.current;
        if (!applyConnection(envelope.data)) return;
        const advanced = previous !== null && envelope.data.generation > previous.generation;
        if (advanced) {
          // 换代：旧代 pending 失效（新投影由重 GET / 后续 session 帧补上）
          setSession(null);
          appliedSessionRef.current = null;
          staleRefreshRef.current = null;
          recoveryRefreshRef.current = null;
          recoveryAttemptsRef.current = { generation: -1, attempts: 0 };
          void refresh();
        } else if (
          envelope.data.state === "paired" &&
          (previous === null || previous.state !== "paired")
        ) {
          // 同一代次内配对完成：代次没变但连接已就绪，必须重拉会话；
          // 已有会话一律保留（只补读，不清视图）。重复 paired（已是 paired）不重拉。
          void refresh();
        }
        return;
      }
      if (name === "session") {
        if (envelope.sessionId !== sessionId) return; // 缺会话维度的会话帧保守丢弃
        if (envelope.generation < generationRef.current.session) return; // 旧代次
        if (!isSessionView(envelope.data)) return;
        // 只有真正落地的帧才触发自愈（旧代次/别的会话一律不刷新）。
        if (applySessionView(envelope.data, envelope.data.ready ? "sse" : "safety-degrade")) {
          requestStaleRefresh(envelope.data);
        }
        return;
      }
      if (name === "action") {
        if (envelope.sessionId !== sessionId) return;
        if (
          envelope.generation <
          Math.max(generationRef.current.connection, generationRef.current.session)
        ) {
          return;
        }
        // 回执帧只作纠偏信号：重拉 GET，不重放动作/不伪造本端回执
        void refresh();
      }
    };

    const connect = () => {
      if (stopped) return;
      if (retryTimer) {
        clearTimeout(retryTimer); // 单定时器纪律：同一时刻只允许一条待重连
        retryTimer = null;
      }
      let next: EventSource;
      try {
        next = new EventSource(zcodeEventsUrl(sessionId));
      } catch {
        return; // 构造失败：GET 首拍仍权威，不做注定失败的退避
      }
      source = next;
      // 帧即恢复证据：任一帧落地才把退避复位（连接刚建立但立刻再失败时不复位，
      // 否则会退化成 1s 恒定的重连热循环）。
      const handle = (name: string, event: Event) => {
        if (stopped || source !== next) return; // 已换连接/已卸载：迟到帧作废
        retryDelay = SSE_RECONNECT_BASE_MS;
        dispatch(name, event);
      };
      next.addEventListener("snapshot", (e) => handle("snapshot", e));
      next.addEventListener("connection", (e) => handle("connection", e));
      next.addEventListener("session", (e) => handle("session", e));
      next.addEventListener("action", (e) => handle("action", e));
      next.addEventListener("resync", (e) => handle("resync", e));
      next.onerror = () => {
        if (stopped || source !== next) return;
        next.close(); // 自管重连：同一次断流只算一次（关流后不再收 error）
        source = null;
        void refresh(); // 一次断流 = 一次纠偏 GET（connection + session 各一发）
        if (retryTimer) clearTimeout(retryTimer);
        const delay = retryDelay;
        retryDelay = Math.min(retryDelay * 2, SSE_RECONNECT_MAX_MS);
        retryTimer = setTimeout(() => {
          retryTimer = null;
          connect();
        }, delay);
      };
    };

    connect();
    return () => {
      stopped = true;
      source?.close();
      source = null;
      if (retryTimer) {
        clearTimeout(retryTimer);
        retryTimer = null;
      }
    };
  }, [sessionId, applyConnection, applySessionView, refresh, requestStaleRefresh]);

  const submit = useCallback(async (action: ControlAction): Promise<ActionReceipt> => {
    // 一次提交 = 一个 actionId：请求体与回执同一个；重试是新提交 → 新 ID。
    const actionId = newActionId();
    if (action.kind === "send") setDraftCleared(false);
    inflightSubmitsRef.current += 1;
    setSubmitting(true);
    setReason(null);
    try {
      const receipt = await postZcodeAction(actionId, action);
      setReason(receiptReason(receipt));
      // 仅 send 的 accepted 允许清本次草稿。`accepted` 已由服务端按 F2 的完整
      // `inputAccepted` 四元组（type/inputId/delivery）验证过——手机侧**绝不再**
      // 自行放宽成「status 看起来像成功」；`confirmed` 只表示已生效，不冒充发送结果。
      if (action.kind === "send" && receipt.state === "accepted") setDraftCleared(true);
      return receipt;
    } catch (error) {
      // 不伪造回执：传输/非 2xx 原样抛出，原因已置为冻结码，草稿保持。
      setReason(errorReason(error));
      throw error;
    } finally {
      inflightSubmitsRef.current -= 1;
      if (inflightSubmitsRef.current === 0) setSubmitting(false);
    }
  }, []);

  return { connection, session, reason, loading, submitting, draftCleared, submit, refresh };
}

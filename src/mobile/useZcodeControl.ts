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
//   只重拉 GET。卸载/切会话关闭流；别的会话或旧代次帧忽略。
// - 阅读链（看板/正文/文件预览）不依赖本 hook 成功。
import { useCallback, useEffect, useRef, useState } from "react";
import {
  ZcodeControlApiError,
  getZcodeConnection,
  getZcodeSession,
  postZcodeAction,
  zcodeEventsUrl,
  type ActionReceipt,
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
   * 最近一次**成功落地**的连接视图（state + generation）：判「同代状态跃迁」用。
   * 只在 applyConnection 真正写入 state 时更新——同代 `needs_auth/connecting → paired`
   * 也必须补拉会话，仅靠代次会漏掉（代次没变的配对完成是合法场景）。
   */
  const appliedConnectionRef = useRef<ConnectionView | null>(null);

  /** 落地连接视图并推进代次；返回 false = 旧代次（不写 state）。 */
  const applyConnection = useCallback((view: ConnectionView): boolean => {
    if (view.generation < generationRef.current.connection) return false;
    generationRef.current.connection = view.generation;
    appliedConnectionRef.current = view;
    setConnection(view);
    return true;
  }, []);

  /** 落地会话投影并推进代次；别的会话/旧代次 → false（不写 state）。 */
  const applySessionView = useCallback((view: SessionView): boolean => {
    if (view.sessionId !== sessionRef.current) return false;
    if (view.generation < generationRef.current.session) return false;
    generationRef.current.session = view.generation;
    setSession(view);
    return true;
  }, []);

  /** 读连接 + 会话并落地；返回是否真正生效（旧会话/旧轮次 → false，不写 state）。 */
  const applyRead = useCallback(
    async (sid: string, run: number): Promise<boolean> => {
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
        if (outcome.ok && isCurrent()) applySessionView(outcome.value);
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

  const refresh = useCallback(async () => {
    const sid = sessionRef.current;
    if (sid === null) return; // 已卸载：静默
    const applied = await applyRead(sid, ++readRunRef.current);
    if (applied) setLoading(false);
  }, [applyRead]);

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
        const sessionApplied = isSessionView(frame.session)
          ? applySessionView(frame.session)
          : false;
        const connectionApplied = connectionView !== null ? applyConnection(connectionView) : false;
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
        if (isSessionView(envelope.data)) applySessionView(envelope.data);
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
  }, [sessionId, applyConnection, applySessionView, refresh]);

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
      // 仅 send 的 accepted 允许清本次草稿；confirmed 只表示已生效，不冒充发送结果。
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

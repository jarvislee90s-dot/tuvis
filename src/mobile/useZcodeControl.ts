// ZCode 统一控制状态 hook（T7.2 Slice A2a）：手机控制面的唯一读/写接驳层。
//
// 纪律（与计划 T7.2 / shared-context 对齐）：
// - 连接态**只**来自 `GET /zcode/connection`：已保存链接、可读会话、成功 GET 都
//   不代表已连接；确认前 `connection=null`，绝不虚构 `paired`。
// - `submit` 只走官方 `POST /zcode/actions`（一次提交一个 `crypto.randomUUID()`
//   actionId；重试 = 新提交 = 新 ID）。**不**回退旧 `session-send`、无头 CLI 或
//   终端按键；unknown 绝不由本 hook 自动重发。
// - 回执状态如实透传：`accepted` 只是宿主已受理（≠ 已生效）；`confirmed` 才是
//   已生效。只有 `send` 的 `accepted` 允许清本次草稿；`send_failed`/`rejected`/
//   `unknown` 一律保留草稿。网络/非 2xx 不伪造回执（抛出，`reason` 已置码）。
// - `refresh` 是后续 SSE 片的纠偏入口：只重拉 GET（连接 + 会话），不置 loading，
//   不清已有视图；迟到的旧会话/旧轮次响应一律作废。
// - 本片**不**建立 EventSource；阅读链（看板/正文/文件预览）不依赖本 hook 成功。
import { useCallback, useEffect, useRef, useState } from "react";
import {
  ZcodeControlApiError,
  getZcodeConnection,
  getZcodeSession,
  postZcodeAction,
  type ActionReceipt,
  type ConnectionView,
  type ControlAction,
  type SessionView,
} from "@/lib/api/zcodeControl";

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

  /** 读连接 + 会话并落地；返回是否真正生效（旧会话/旧轮次 → false，不写 state）。 */
  const applyRead = useCallback(async (sid: string, run: number): Promise<boolean> => {
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
      if (outcome.ok && isCurrent()) setConnection(outcome.value);
    });
    void sessionRead.then((outcome) => {
      if (outcome.ok && isCurrent()) setSession(outcome.value);
    });
    const [connectionResult, sessionResult] = await Promise.all([connectionRead, sessionRead]);
    if (!isCurrent()) return false;
    // 原因面等两面都到齐再定：连接错误优先，连接正常时取会话错误。
    let nextReason = connectionResult.ok
      ? connectionReason(connectionResult.value)
      : errorReason(connectionResult.error);
    if (!sessionResult.ok && nextReason === null) {
      nextReason = errorReason(sessionResult.error);
    }
    setReason(nextReason);
    return true;
  }, []);

  useEffect(() => {
    let alive = true;
    sessionRef.current = sessionId;
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

  const submit = useCallback(async (action: ControlAction): Promise<ActionReceipt> => {
    // 一次提交 = 一个 actionId：请求体与回执同一个；重试是新提交 → 新 ID。
    const actionId = crypto.randomUUID();
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

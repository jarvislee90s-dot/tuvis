// T05：页面共享的 Codex 回合停止控制器（spec §5「发送与停止」）。
//
// - **单例持有（SessionDetail 层）**：同一页面上的多个停止入口（composer 主按钮 /
//   composer 紧凑入口 / 队列面板打断确认）共用同一份控制器实例——`stopping` 期间
//   `stop()` 为 no-op，避免多按钮各发一次破坏性打断；
// - **目标冻结**：`stop()` 在**点击时刻**冻结当时的 `activeTurnId`（turn A）并随
//   请求发出——请求在飞期间镜像出现新回合（turn B）不得改写目标（旧请求不能停止
//   B；服务端另有 expectedTurnId 安全阀兜底）；
// - **复用既有定向打断 API**（`interruptCodexTurn`，2026-10-21 端点）：不新造端点；
//   结果分诊照抄既有消费方（CodexQueuePanel.interruptTurn）的语义——
//   uncertain=true = 指令已发出但结果未知（不报失败、不诱导重按）；
//   interruptedTurnId 非空 = 确实打断；null = 桌面无可打断回合（不谎报成功）；
//   抛错 = 停止失败（可读化）；
// - **30 秒前台预算**（S3 前台统一预算的同款量级）：前端本地单调计时，到期强制
//   结束等待并按「结果不确定」呈现——**不把「停止 HTTP 等待」冒充停止模型回合**；
//   迟到的原始响应一律作废（预算已移交呈现）；
// - **Unknown 纪律**：不自动重试、不自动接着发稿——控制器只产出结果状态，重试/
//   续发的决策权完全留给用户（界面上也不提供自动路径）；
// - **canStop 三元组缺一不可**（T03 事实）：新鲜投影（非 stale）+ runtimeStatus
//   "active" + effectiveOperations.interrupt.allowed + activeTurnId 非空。任一缺失
//   = 不可执行停止（按钮可见性由消费方按 spec §5 表格就近降级为短状态）；
// - **R02 资格门进控制器**：stop() 开头自查「canStop 语义 + 冻结目标非空」，不满足
//   零请求返回 refused——所有消费入口（composer 主按钮/紧凑入口/队列面板确认臂）
//   复用同一道门，不依赖按钮隐藏；队列入口的 canInterrupt 来自独立 queue GET，
//   投影 stale 时它仍可能为 true，门必须在控制器这一侧兜住（不发不带 turnId 的
//   请求、不回退服务器当前回合）；
// - **R02 计时器归属**：30 秒预算计时器由每次 stop 局部创建的 handle + 提交代际
//   共同拥有，timeout 回调与 finally 只清/判定自己的 handle——旧请求的迟到 finally
//   不得清掉后继停止的计时器（否则后继永久 stopping）。
import { useCallback, useEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import { interruptCodexTurn, type InterruptTurnResult, type ThreadProjection } from "./codexIpcApi";
import { ApiError } from "./api";
import { formatInvokeError } from "../lib/invokeError";

/** 停止结果（与 interruptCodexTurn 回执三分支 + 失败一一对齐；UI 据此如实呈现）。
 *  R02 增补 `refused`：控制器资格门拦下（零请求）——`no_target` = 冻结目标为空
 *  （投影 stale/目标不明，绝不回退成不带 turnId 的请求）；`not_allowed` = 目标
 *  在场但资格缺失（非运行/未获准 interrupt）。纯增量：旧消费方的 switch 需补臂。 */
export type CodexTurnStopResult =
  | { kind: "interrupted"; turnId: string | null }
  | { kind: "no_active_turn" }
  | { kind: "uncertain"; reasonCode: string | null }
  | { kind: "failed"; message: string }
  | { kind: "refused"; reason: "no_target" | "not_allowed" };

/** 控制器对外契约（跨任务固定；T06 增补 `activeTurnId` 为纯增量——旧消费方不受影响）：
 *  输入当前投影 + sessionId，输出以下六件。 */
export interface CodexTurnControl {
  /** 可执行停止：三元组齐备（新鲜 + 运行中 + interrupt.allowed + 有目标） */
  canStop: boolean;
  /** 模型回合运行中（最后已知运行态，含断链保留视图）——与发送 HTTP busy
   *  （sending）是两个状态，不混用 */
  turnRunning: boolean;
  /** 停止请求在飞（含 30 秒预算窗）；期间 stop() 为 no-op */
  stopping: boolean;
  /** T06：最近已知活动回合 id（仅投影新鲜时非空，与 canStop 的目标同源）——
   *  优先处理确认臂在**确认点击时刻**冻结后随 prioritize 请求发出
   * （expectedTurnId 安全阀）；与 stop() 的点击冻结纪律一致。 */
  activeTurnId: string | null;
  /** 最近一次停止的结果（分诊后；null = 尚无/已清） */
  result: CodexTurnStopResult | null;
  /** 发起停止：冻结点击时刻的 activeTurnId；返回分诊结果。
   *  拦下面：stopping 在飞 → null（no-op）；资格门不过（目标为空/资格缺失）→
   *  refused 结果（零请求）。 */
  stop: () => Promise<CodexTurnStopResult | null>;
  /** 清除结果状态（下一轮呈现从此开始；不自动调用——Unknown 不因界面收纳而消失） */
  clearResult: () => void;
}

/** 30 秒前台预算（毫秒）：与 MessageComposer 的 FRONT_DEADLINE_MS 同量级——
 *  停止指令的 RPC 响应挂起不能无限转圈；到期按「结果不确定」呈现（指令可能已
 *  到达桌面端，重按 = 双发破坏性指令）。 */
const STOP_DEADLINE_MS = 30_000;

/** InterruptTurnResult → 停止结果分诊（照抄 CodexQueuePanel.interruptTurn 的三分支；
 *  单源导出：共享控制器与面板的非共享回退路径共用，不复制分诊逻辑）。 */
export function triageInterruptResult(res: InterruptTurnResult): CodexTurnStopResult {
  if (res.uncertain) return { kind: "uncertain", reasonCode: res.reasonCode ?? null };
  if (res.interruptedTurnId !== null) return { kind: "interrupted", turnId: res.interruptedTurnId };
  return { kind: "no_active_turn" };
}

/** 停止结果 → 用户可读文案（复用队列面板既有键——同一事实两种入口措辞不漂移；
 *  文案如实：uncertain 不报失败不诱导重按、no_active_turn 不谎报成功、refused
 *  如实说明「未发出任何请求」的原因）。 */
export function codexTurnStopCopy(result: CodexTurnStopResult): string {
  switch (result.kind) {
    case "interrupted":
      return i18n.t("mobile.codex.queue.interruptDone");
    case "uncertain":
      return i18n.t("mobile.codex.queue.interruptUncertain");
    case "no_active_turn":
      return i18n.t("mobile.codex.queue.interruptNoTurn");
    case "failed":
      return i18n.t("mobile.codex.queue.interruptFailed", { message: result.message });
    case "refused":
      return result.reason === "no_target"
        ? i18n.t("mobile.codex.queue.interruptNoTarget")
        : i18n.t("mobile.codex.queue.interruptNotAllowed");
  }
}

/** ApiError → 可读分诊片段（与 CodexQueuePanel.apiErrorReason 同一条回退链：
 *  后端 reason → 机器码 error → message → formatInvokeError 按码映射） */
function readableStopError(e: ApiError): string {
  const raw =
    typeof e.data?.reason === "string" && e.data.reason !== ""
      ? e.data.reason
      : typeof e.data?.error === "string" && e.data.error !== ""
        ? e.data.error
        : e.message;
  return formatInvokeError(raw, i18n.t);
}

export function useCodexTurnControl(
  projection: ThreadProjection | null | undefined,
  sessionId: string
): CodexTurnControl {
  const [stopping, setStopping] = useState(false);
  const [result, setResult] = useState<CodexTurnStopResult | null>(null);
  /** stopping 的同步镜像（双击在同一渲染帧内进入两次 stop() 时，state 闭包里
   *  的旧值仍是 false——ref 同步读写，第二次必然命中第一次写入的闸） */
  const stoppingRef = useRef(false);
  /** 停止代际：30 秒预算到期后迟到的原始响应一律作废（不得覆盖已呈现的不确定） */
  const epochRef = useRef(0);
  /** 预算计时器把手（卸载/换会话清理兜底） */
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** 会话哨兵：响应落地时会话已切换即作废（旧会话的停止回执不落新会话）。
   *  赋值在 sessionId effect 内（渲染期不得写 ref——react-hooks/refs 纪律）；
   *  effect 先于任何用户点击运行，stop() 读取时已是最新会话。 */
  const sessionRef = useRef(sessionId);

  // ===== canStop 三元组（缺一不可执行；T03 事实，前端不猜不放宽）=====
  // - turnRunning = 最后已知运行态（runtimeStatus "active"）：断链保留的最后视图
  //   仍如实报告「运行中」——但 stale 时绝不据此承诺可执行停止；
  // - interrupt.allowed（实时资格门在后端；旧载荷缺 effectiveOperations = 未知 →
  //   不可执行，不从 availability 推导）；
  // - activeTurnId 非空且投影新鲜（服务端仅在新鲜镜像同次观察到的活动回合才下发；
  //   stale 保留视图的目标一律不作为停止依据）。
  // canStop = turnRunning ∧ fresh ∧ interruptAllowed ∧ 有目标——任一缺失 = 不可
  // 执行停止；turnRunning 而 canStop=false 的组合（断链/目标不明/能力不足）由
  // 消费方就近显示短状态（spec §5 最后行）。
  const fresh = projection != null && projection.stale !== true;
  const turnRunning = projection?.runtimeStatus === "active";
  const interruptAllowed = projection?.effectiveOperations?.operations?.interrupt?.allowed === true;
  const activeTurnId =
    fresh && typeof projection.activeTurnId === "string" && projection.activeTurnId.length > 0
      ? projection.activeTurnId
      : null;
  const canStop = turnRunning && fresh && interruptAllowed && activeTurnId !== null;

  const clearResult = useCallback(() => setResult(null), []);

  const stop = useCallback(async (): Promise<CodexTurnStopResult | null> => {
    // 双击/多入口并发闸：stopping 期间一律 no-op（共享单例的存在的理由）
    if (stoppingRef.current) return null;
    // R02 资格门（控制器自身把关，不依赖消费方按钮隐藏）：与 canStop 同一语义 +
    // 冻结目标非空。不满足 → 零请求返回 refused——绝不回退成「不带 turnId 的请求，
    // 让服务器选它当时的回合」（队列入口的 canInterrupt 来自独立 queue GET，投影
    // stale 时它仍可能为 true，门必须在目标这一侧兜住）。
    if (activeTurnId === null) return { kind: "refused", reason: "no_target" };
    if (!canStop) return { kind: "refused", reason: "not_allowed" };
    const sid = sessionId;
    // **点击时刻冻结目标**：此后投影变化（镜像出现 turn B）不影响本次请求
    const frozenTurnId = activeTurnId;
    stoppingRef.current = true;
    setStopping(true);
    setResult(null);
    const myEpoch = ++epochRef.current;
    // 30 秒前台预算：前端本地单调计时，到期强制结束等待 → 按「不确定」呈现。
    // R02 计时器归属：handle 由本次 stop 局部创建，与提交代际（myEpoch）共同
    // 拥有——timeout 回调与 finally 只清/判定自己的 handle；timerRef 只作
    // 卸载/换会话的清理登记簿（仅当仍登记着自己的 handle 才清登记），旧请求的
    // finally 不再触碰共享 ref 上别人的计时器（否则后继停止的预算被误清，
    // 后继永久 stopping——审查复现：30,001ms 后 stopping 仍 true）。
    let timedOut = false;
    const myTimer = setTimeout(() => {
      if (timerRef.current === myTimer) timerRef.current = null;
      timedOut = true;
      if (epochRef.current !== myEpoch || sessionRef.current !== sid) return;
      stoppingRef.current = false;
      setStopping(false);
      setResult({ kind: "uncertain", reasonCode: "timeout" });
    }, STOP_DEADLINE_MS);
    timerRef.current = myTimer;
    try {
      const res = await interruptCodexTurn(sid, frozenTurnId);
      // 预算已到期 / 代际已前进 / 会话已切换 → 迟到响应作废（不覆盖已呈现状态）
      if (timedOut || epochRef.current !== myEpoch || sessionRef.current !== sid) return null;
      const triaged = triageInterruptResult(res);
      setResult(triaged);
      return triaged;
    } catch (e) {
      if (timedOut || epochRef.current !== myEpoch || sessionRef.current !== sid) return null;
      // 分诊照抄既有消费方：抛错 = 停止失败（可读化）；不归入 uncertain
      //（那会让用户误以为指令可能已生效）
      const message = e instanceof ApiError ? readableStopError(e) : String(e);
      const failed: CodexTurnStopResult = { kind: "failed", message };
      setResult(failed);
      return failed;
    } finally {
      // 只清自己的 handle（已触发/已被会话切换清理的 handle 再清是无害 no-op）
      clearTimeout(myTimer);
      if (timerRef.current === myTimer) timerRef.current = null;
      // 预算到期路径已在计时器里收尾（stopping 复位）；这里只收正常完成路径
      if (!timedOut && epochRef.current === myEpoch && sessionRef.current === sid) {
        stoppingRef.current = false;
        setStopping(false);
      }
    }
  }, [activeTurnId, canStop, sessionId]);

  // 换会话：清结果与在飞标记（旧会话的停止现场不带入新会话；在途响应经会话哨兵
  // 作废），并前移会话哨兵（渲染期不写 ref——react-hooks/refs 纪律；effect 先于
  // 任何用户点击运行，stop() 读取时已是最新会话）
  useEffect(() => {
    sessionRef.current = sessionId;
    epochRef.current += 1;
    stoppingRef.current = false;
    setStopping(false);
    setResult(null);
    return () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, [sessionId]);

  // 卸载兜底：清掉真实挂在事件循环上的预算计时器
  useEffect(
    () => () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    },
    []
  );

  return { canStop, turnRunning, stopping, activeTurnId, result, stop, clearResult };
}

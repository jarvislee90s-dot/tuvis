// Codex 待发队列面板（Task 9）：本设备在当前 Thread 的动作账本视图。
//
// - 显示本设备待发与**实际 FIFO 位置**（position 来自服务端 codex_actions 的
//   (thread_id,state,seq) 排序——跨设备真队列序，不是本端发送顺序）；
// - 撤回：仅 queued 可撤（queued→cancelled CAS）。成功 → 操作后重拉（权威列表
//   收敛，条目消失）；409（已派发）→ 条目保留 + 权威刷新 + 说明——绝不谎报
//   撤回成功；404（不存在/他人动作）→ 重拉对账；
// - blockedReasonCode 非空（如 compat_outstanding）= 队列暂停派发：说明条可见，
//  不让用户盲等（types.rs CodexQueueView 的字段注释即本条契约）；
// - **人工解除无出口屏障（2026-10-19 复审 Important）**：native_delivery_unknown /
//   compat_outstanding 两类无自动恢复路径的屏障，提供「已到桌面端核对，继续队列」
//   按钮——**两步确认**（未确认不发任何请求；文案如实披露重复内容风险，并披露
//   混合屏障面：清单含等待 Turn 完成的条目时将提前解除其顺序限制——行清单不带
//   state，前端无从逐行甄别，只能文案如实）后逐个解除 blockedActionIds 并重拉；
//   单个 id 的确定性「无需解除」回执（409 no_barrier——屏障已被自动路径解除/
//   从未在册；409 barrier_in_flight——屏障在册但派发在飞有自动路径）只收尾该 id
//   不中断剩余；真失败显示错误不静默（note 走 reason →
//   机器码 → message 可读化回退链）。native_turn_pending 有自动恢复路径（Turn
//   终态证据驱动），不提供该按钮；
// - 停止当前回合仅由 composer 控制；本面板保留 Unknown 核对和暂停恢复。
// - **插队语义边界（2026-10-29 T06 收紧为「显式单条优先」）**：本面板没有通用
//   「立即发送/跳到队首」或拖动排序入口（宪法 R2.3 忙态留队、FIFO 按序）；唯一
//   越队出口 = T06「插队」——仅对**具体 queued 行**显式触发（两步确认：
//   运行中先确认停止当前回合，暂停空闲仅确认本条越队），服务端经一次性越队授权
//   恰好派发一帧、旧队列保持暂停，不接受正文副本。cancelled/终态条目也不出现
//   重试按钮（重投 = 同 actionId 双投风险，绝不自动）；
// - **刷新统一源（R11）**：面板除挂载/换会话拉取外，还接受 `refreshSignal` 外部
//   触发（投影跃迁 / 发送入队回执，由 SessionDetail 汇聚为同一 tick 下发）——
//   否则「空队列时发送 codex_queued」面板仍自隐、SSE 推进后队位不刷新（新入队
//   消息在当前详情页不可见不可撤，违反宪法 F2.2）。同会话密集触发去抖合并为
//   一次重拉；重拉带请求序号 + 会话哨兵，旧会话/过期响应一律不回灌。
import { useCallback, useEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import {
  fetchCodexQueue,
  forceContinueCodexAction,
  editCodexQueueMessage,
  prioritizeCodexQueueMessage,
  resumeCodexQueue,
  retractCodexAction,
  type CodexActionState,
  type CodexPrioritizeResult,
  type CodexQueueView,
} from "./codexIpcApi";
import { ApiError } from "./api";
import { formatInvokeError } from "../lib/invokeError";
// P4：正文预览共享词法（剥尾签名 + 附件占位）——与 MessageComposer 同一份正则，
// 不复制两份（两处口径漂移会让同一正文在发送区与队列剥出不同结果）
import { previewBody } from "./messagePreview";
// T05：页面共享停止控制器（SessionDetail 单例；打断确认经它发起——与 composer
// 停止入口共用 stopping 互斥闸，多按钮不会各发一次打断）。分诊/文案单源复用。
import type { CodexTurnControl } from "./useCodexTurnControl";

/** 外部触发的去抖窗口（毫秒）：同一会话内的密集触发（发送回执 + 投影跃迁几乎
 *  同时到达）合并为一次重拉；拉取本身是会话级短查询，延迟一拍无感 */
const QUEUE_REFRESH_DEBOUNCE_MS = 250;
/** C6：去抖**上限等待**（毫秒）：流式输出每 ~100ms 推进一次 revision，若每次
 *  都重置去抖，入队回执触发的刷新会被无限推后（条目直到输出暂停才可见/可撤）。
 *  自首个未处理触发起超过本上限即立即重拉——合并与及时性两头保住。 */
const QUEUE_REFRESH_MAX_WAIT_MS = 2000;
/** 插队收尾和核对读取的时限，超时的快照由 seq/session 哨兵作废。 */
const QUEUE_OP_REFRESH_TIMEOUT_MS = 2000;
const QUEUE_OBSERVE_INTERVAL_MS = 2000;
const QUEUE_OBSERVE_WINDOW_MS = 12000;
const MANUAL_STOP_PAUSE_RECOVERY_MS = 30_000;

/** 动作状态 → 展示词（snake_case wire 词 → 用户可读；unknown 如实「结果未知」） */
function stateLabel(s: CodexActionState): string {
  switch (s) {
    case "queued":
      return i18n.t("mobile.codex.queue.stateQueued");
    case "reserved":
      return i18n.t("mobile.codex.queue.stateReserved");
    case "accepted":
      return i18n.t("mobile.codex.queue.stateAccepted");
    case "rejected":
      return i18n.t("mobile.codex.queue.stateRejected");
    case "unknown":
      return i18n.t("mobile.codex.queue.stateUnknown");
    case "cancelled":
      return i18n.t("mobile.codex.queue.stateCancelled");
  }
}

// ============================================================
// P4：队列行正文预览（textPreview 展示规约）
// ============================================================

/** P4 预览显示长度（按字符数，非字节）：比 composer 队列条（12 字符）更长的一行 */
const QUEUE_ROW_PREVIEW_MAX_CHARS = 40;

/** 队列行正文预览（P4）：textPreview 是 queued_text 原文（**未剥签名**，服务端已按
 *  字符边界截断 ≤120 字符）。展示前处理：剥尾部移动端签名 → 附件标记替换可读
 *  占位（正则单源 messagePreview，与 composer 同一份）→ 换行折叠空格归一单行
 *  （窄屏不撑破）→ 截前 40 字符 + 省略号（按字符数）。 */
function queueRowPreview(textPreview: string): string {
  const singleLine = previewBody(textPreview)
    .replace(/\s*[\r\n]+\s*/g, " ")
    .trim();
  return singleLine.length > QUEUE_ROW_PREVIEW_MAX_CHARS
    ? `${singleLine.slice(0, QUEUE_ROW_PREVIEW_MAX_CHARS)}…`
    : singleLine;
}

/** P6：该条正文的**真实长度**（字符数，剥签名后、不截断）——用户要知道队列里
 *  这条到底多长；若用预览截断后的长度（恒 ≤40）等于谎报。 */
function queueRowLength(textPreview: string): number {
  return previewBody(textPreview)
    .replace(/\s*[\r\n]+\s*/g, " ")
    .trim().length;
}

// ============================================================
// T06：已入队消息的显式优先处理（「插队」）
// ============================================================

/** T06 not_sent 理由码的可读化（词表内映射；词表外保守可读兜底——不裸码、不
 *  臆测精确原因/是否写出，wire 的 queued 事实由外层 wrapper 如实陈述）。
 *
 *  Task9（队列结论分型 RED→GREEN）：拒绝/失败原因必须说**本条实际发生了什么**，
 *  不能把互斥忙、停止链路各环节失败全部吞成一句「桌面正忙」。分型对照后端
 *  `queue_prioritize` 的真实 reasonCode 词表（codex_ipc.rs / router.rs）：
 *  - `dispatch_in_flight`：**其他**发送/接管在途（并发派发互斥），本条仍 queued——
 *    与「桌面正忙」分型（后者指向「等一会儿就好」，对并发互斥是误导）；
 *  - `action_cancelled`：动作已被撤回离队（queued=false），不宣称仍在队列；
 *  - `desktop_rejected`：桌面明确拒绝（receipt.delivery=rejected 的最终回执）；
 *  - `device_revoked`：设备授权失效——两形态分文案（queued=false 落账回执 =
 *    已发送失败且离队；queued=true = 接管复核竞态留队，不断言已离队）；
 *  - `expected_turn_required`：确认停止缺目标回合阀值（零停止零越队）；
 *  - `stop_desktop_rejected` / `stop_disconnected` / `stop_stale_context` /
 *    `stop_claim_unavailable` / `stop_device_revoked` /
 *    `stop_capability_unavailable`：停止链路各失败点（桌面拒绝 / 连接断开 /
 *    上下文换代 / 占有登记失败 / 设备失效 / 能力未启用），各给对应文案；
 *  - `stop_not_effective`：未证明目标回合终态——**不断言回合仍在运行**（缺的
 *    是终态证据，回合可能已换代），只说「未能确认已停止」；
 *  - `front_budget_elapsed`：预算等待超时——**不带停止语义**（该码也可来自
 *    消息派发阶段的新鲜门等待，idle 零停止场景同样会产生）；
 *  - `store_error`：本地账本读写暂时失败（保守留队）；
 *  - `queued`：本轮未轮到执行（不臆测原因）；`grant_consumed`：授权缺失或已
 *    消费两义并存——保守「暂无法继续 + 核对」，不断言已发送/必然已消费。 */
function prioritizeRefusalCopy(reasonCode: string, queued: boolean): string {
  switch (reasonCode) {
    case "interrupt_confirmation_required":
      return i18n.t("mobile.codex.queue.prioritizeNeedsStopConfirm");
    case "native_pending":
      return i18n.t("mobile.codex.queue.prioritizeNativePending");
    case "state_unknown":
      return i18n.t("mobile.codex.queue.prioritizeStateUnknown");
    case "expected_turn_required":
      return i18n.t("mobile.codex.queue.prioritizeExpectedTurn");
    case "stop_not_effective":
      return i18n.t("mobile.codex.queue.prioritizeStopNotEffective");
    case "front_budget_elapsed":
      // 不带停止语义：消息派发阶段的新鲜门等待超时同样产生此码（idle 零停止）
      return i18n.t("mobile.codex.queue.prioritizeBudgetTimeout");
    case "stop_timeout":
      return i18n.t("mobile.codex.queue.prioritizeStopUncertain");
    case "unknown_delivery_barrier":
      // P2：投递结果未知挡住队列 ≠ 桌面正忙——后者对前者是误导（用户会以为等
      // 一会儿就好，实际需要人工核对 Unknown 那条）。如实分型：指引核对队列/
      // 回桌面核对后「继续队列」。
      return i18n.t("mobile.codex.queue.prioritizeUnknownBarrier");
    case "barrier_outstanding":
    case "busy_before_write":
      // 这两码才是「桌面忙、稍后再试」语义（barrier_outstanding=未解屏障挡队列，
      // busy_before_write=写前冻结回队）；dispatch_in_flight 已分型拆出。
      return i18n.t("mobile.codex.queue.prioritizeBusy");
    case "dispatch_in_flight":
      return i18n.t("mobile.codex.queue.prioritizeDispatchInFlight");
    case "action_cancelled":
      return i18n.t("mobile.codex.queue.prioritizeActionCancelled");
    case "desktop_rejected":
      return i18n.t("mobile.codex.queue.prioritizeDesktopRejected");
    case "device_revoked":
      return queued
        ? i18n.t("mobile.codex.queue.prioritizeDeviceRevokedQueue")
        : i18n.t("mobile.codex.queue.prioritizeDeviceRevoked");
    case "stop_desktop_rejected":
      return i18n.t("mobile.codex.queue.prioritizeStopDesktopRejected");
    case "stop_disconnected":
      return i18n.t("mobile.codex.queue.prioritizeStopDisconnected");
    case "stop_stale_context":
      return i18n.t("mobile.codex.queue.prioritizeStopStaleContext");
    case "stop_claim_unavailable":
      return i18n.t("mobile.codex.queue.prioritizeStopClaimUnavailable");
    case "stop_device_revoked":
      return i18n.t("mobile.codex.queue.prioritizeStopDeviceRevoked");
    case "stop_capability_unavailable":
      return i18n.t("mobile.codex.queue.prioritizeStopCapabilityUnavailable");
    case "store_error":
      return i18n.t("mobile.codex.queue.prioritizeStoreError");
    case "queued":
      // 字面「queued」码：只说「本次未优先执行」，不臆测原因（排队事实由
      // wrapper 的「仍在队列」按 wire queued=true 如实陈述）
      return i18n.t("mobile.codex.queue.prioritizeQueuedNotExecuted");
    case "grant_consumed":
      // 授权缺失/已消费两义并存：保守「暂无法继续 + 核对」，不断言已发送或
      // 必然已消费
      return i18n.t("mobile.codex.queue.prioritizeGrantUnavailable");
    default:
      // 词表外拒绝码：保守可读兜底（不裸示机器码、不编造语义）
      return i18n.t("mobile.codex.queue.prioritizeRefusalUnknownReason");
  }
}

/** Unknown 结果的「阶段事实」文案：`unknown_delivery` = 投递结果未知；已知
 *  `stop_` 前缀 = 停止结果未确认（与投递未知明确区分——两者都「不自动重试」，
 *  但用户要核对的桌面状态不同）；其余（`front_budget_elapsed` 等无法凭码定性
 *  阶段的）→ 保守「结果未知」。绝不凭码 + 排队快照反推「一定是停止等待」。 */
function prioritizeUnknownPhaseCopy(reasonCode: string): string {
  if (reasonCode === "unknown_delivery") {
    return i18n.t("mobile.codex.queue.prioritizeDeliveryUnknown");
  }
  if (reasonCode.startsWith("stop_")) {
    return i18n.t("mobile.codex.queue.prioritizeStopUnknownPhase");
  }
  return i18n.t("mobile.codex.queue.prioritizeResultUncertain");
}

/** 操作后成功 GET 的同 scope/action 快照 → 本条排队**正**事实。
 *
 *  真实 /codex/queue 只 list_queued：快照只能正面证明「同 action 仍排队 + 队
 *  位」。目标缺席（操作后离队或列表滞后）与刷新失败/乱序一律返回空串——缺席
 *  **不**证明已投递/未投递/已离队，也不从 thread blockedReasonCode 反推本条
 *  delivery；不编造 wire 不存在的 queued/receipt，不用旧闭包视图补状态。 */
function queueFactFromSnapshot(view: CodexQueueView | null, actionId: string): string {
  const mine = view?.actions.find((a) => a.actionId === actionId);
  if (!mine || mine.state !== "queued") return "";
  return mine.position !== null && mine.position >= 1
    ? i18n.t("mobile.codex.queue.noteQueuedPosition", { n: mine.position })
    : i18n.t("mobile.codex.queue.noteQueued");
}

/** prioritize HTTP/网络错误 → 可读保守分型（Task9）：网络异常（status=null）、
 *  服务端失败（internal / 5xx）按人类可读「服务/连接失败 + 先回桌面核对」陈述，
 *  **不宣称队列状态**（HTTP 错误缺 queued 事实，不默认「仍在队列/未写出/已送
 *  达」），不建议直接重发；`forbidden`（权限/设备门）与 `capability_unavailable`
 *  （能力门，**不附会「连接中断」**）按码换文案；词表外也走保守可读兜底，
 *  绝不裸示机器码。wire 的 status/code 由 ApiError 原样保留（诊断不丢）。 */
function prioritizeHttpErrorCopy(e: ApiError): string {
  if (e.status === null) return i18n.t("mobile.codex.queue.prioritizeErrNetwork");
  const code = typeof e.data?.error === "string" ? e.data.error : "";
  if (code === "forbidden") return i18n.t("mobile.codex.queue.prioritizeErrForbidden");
  if (code === "capability_unavailable")
    return i18n.t("mobile.codex.queue.prioritizeErrCapability");
  if (code === "internal" || e.status >= 500)
    return i18n.t("mobile.codex.queue.prioritizeErrInternal");
  return i18n.t("mobile.codex.queue.prioritizeErrGeneric");
}

/** 插队回执文案。调用方核对 actionId 和当前队列快照后再选用。 */
function prioritizeResultCopy(res: CodexPrioritizeResult): string {
  switch (res.result) {
    case "prioritized":
      return i18n.t("mobile.codex.queue.prioritizeDone");
    case "already_dispatched":
      return i18n.t("mobile.codex.queue.prioritizeAlready", {
        state: stateLabel(res.action.state),
      });
    case "not_sent":
      if (res.queued) {
        return i18n.t("mobile.codex.queue.prioritizeRefused", {
          reason: prioritizeRefusalCopy(res.reasonCode, true),
        });
      }
      return i18n.t("mobile.codex.queue.prioritizeFailed", {
        message: prioritizeRefusalCopy(res.reasonCode, false),
      });
    case "unknown":
      return prioritizeUnknownPhaseCopy(res.reasonCode);
  }
}

/** ApiError → 可读分诊片段：后端 reason（人类可读，如「该消息仍在排队，请使用
 *  撤回」）优先，其次机器码 error（如 no_barrier / queued_use_retract），最后兜底
 *  e.message——对齐 PendingRequestCard C5 臂的回退链，不把「路径 + 状态码」裸示人。
 *  带码 reason（错误码门禁约定）过 formatInvokeError 按码换 errors.* 文案；
 *  未知码/无码原样返回。 */
function apiErrorReason(e: ApiError): string {
  const raw =
    typeof e.data?.reason === "string" && e.data.reason !== ""
      ? e.data.reason
      : typeof e.data?.error === "string" && e.data.error !== ""
        ? e.data.error
        : e.message;
  return formatInvokeError(raw, i18n.t);
}

/** 该次 force-continue 失败是否「该 id 无需解除」的确定性收尾（2026-10-19 复审
 *  Minor：一败全停修复）——两个确定性「无需人工解除」机器码都收尾该 id 不中断：
 *  409 no_barrier = 屏障已被自动路径解除/从未在册（典型：视图与确认之间屏障被
 *  自动路径清掉）；409 barrier_in_flight = 屏障在册但该 id 派发在飞（有自动落账
 *  路径，天然不需要人工解除）。两者剩余未解除 id 都与该 id 无关，循环继续逐个
 *  解除；仅真失败（网络异常 / 5xx / 403 门禁等）才中断（此时剩余屏障状态不可知，
 *  重拉后由用户再试）。200（含 AlreadyReleased 幂等回执，HTTP 层与普通解除不可分）
 *  天然继续。409 queued_use_retract 不在此列：它指引「走撤回」，按真失败中断并由
 *  note 可读化转述（且屏障在册清单不含 queued 行，本面板实际到不了该臂）。 */
function isPerIdSettledRefusal(e: unknown): boolean {
  return (
    e instanceof ApiError &&
    e.status === 409 &&
    (e.data?.error === "no_barrier" || e.data?.error === "barrier_in_flight")
  );
}

export default function CodexQueuePanel({
  sessionId,
  refreshSignal = 0,
  onQueueStateChanged,
  turnControl,
  onEditDraft,
  keepEditDraftOnUnmount = false,
}: {
  onEditDraft?: (draft: { sessionId: string; actionId: string; text: string }) => void;
  /** 由按会话持有草稿的父组件接收已成功取回的正文。 */
  keepEditDraftOnUnmount?: boolean;
  sessionId: string;
  /** R11：外部刷新信号（单调递增 tick）。每次递增 → 去抖重拉一次队列视图；
   *  触发源：a) 投影跃迁（SessionDetail）；b) 发送入队回执（MessageComposer）。
   *  c) 面板自身撤回完成不经此信号（内部直接重拉）。 */
  refreshSignal?: number;
  /** F8：队列暂停态可能翻转的通知口（打断受理 / 恢复完成后调用）——SessionDetail
   *  据此触发同页 composer 重拉 send-info（否则打断后「本条先发送」二选一不出，
   *  普通发送又被服务端 queue_paused 拒绝）。 */
  onQueueStateChanged?: () => void;
  /** 页面共享停止状态：插队确认冻结回合目标，停止在飞时锁住行操作。 */
  turnControl?: CodexTurnControl;
}) {
  const [view, setView] = useState<CodexQueueView | null>(null);
  const [manualStopPauseRecovery, setManualStopPauseRecovery] = useState(false);
  const stopPauseAutoResumes = view?.queuePaused === true && view.stopPauseAutoResumes === true;
  useEffect(() => {
    setManualStopPauseRecovery(false);
    if (!stopPauseAutoResumes) return;
    const timer = setTimeout(() => setManualStopPauseRecovery(true), MANUAL_STOP_PAUSE_RECOVERY_MS);
    return () => clearTimeout(timer);
  }, [sessionId, stopPauseAutoResumes]);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  /** 人工解除的二次确认臂（未确认不发任何请求——与 ModeBarCli 同款前端防线） */
  const [forceArmed, setForceArmed] = useState(false);
  /** 人工解除进行中（逐个解除 blockedActionIds 期间禁用全部按钮） */
  const [forceBusy, setForceBusy] = useState(false);
  /** S5：恢复队列进行中（POST /codex/queue/resume = 打断后持久暂停的唯一解除出口） */
  const [resumeBusy, setResumeBusy] = useState(false);
  /** T06：优先处理的确认臂（哪个 queued 行处于确认态；未确认不发任何请求） */
  const [prioritizeArmedId, setPrioritizeArmedId] = useState<string | null>(null);
  /** 每次挂载默认展开。收起只隐藏界面，刷新和空队列自隐都保留用户选择，
   *  确认状态、在途操作和提示也继续保存在组件中。 */
  const [expanded, setExpanded] = useState(true);
  /** T06：优先处理请求在飞（按行禁用；回执关联原 actionId） */
  const [prioritizeBusyId, setPrioritizeBusyId] = useState<string | null>(null);
  const operationBusy = busyId !== null || forceBusy || resumeBusy || prioritizeBusyId !== null;
  /** 乱序防护（R11）：请求序号——每次发起 +1，响应落地时不再是最新序号即作废
   *  （先发后至的响应不得覆盖后发请求的结果） */
  const seqRef = useRef(0);
  /** 乱序防护：会话哨兵——响应落地时会话已切换即作废（旧 session 结果不回灌） */
  const sessionRef = useRef(sessionId);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** C6：首个未处理触发的时刻（maxWait 基准；重拉完成即清） */
  const firstPendingAtRef = useRef<number | null>(null);
  /** T06：prioritize 操作代际（有效 armed 确认 +1；scope 卸载/换会话 +1）。
   *  当前操作判据 = sessionRef===sid && prioritizeOpRef===op 双验：只有当前 op
   *  可 setNote/清 busy/通知父；旧 op 直接失权——新 scope setup 已同步重置其
   *  锁与状态，不依赖已失权旧 op 的清理，避免残锁带进新 scope。 */
  const prioritizeOpRef = useRef(0);
  // 已受理是终态。仅用匹配原 actionId 的回执记账，挡住同一会话的旧排队快照。
  const acceptedIdsRef = useRef(new Set<string>());
  const prioritySnapshotRef = useRef<((view: CodexQueueView) => void) | null>(null);
  const observationTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const observationReadRef = useRef<number | null>(null);
  const queueReadRef = useRef<AbortController | null>(null);

  const stopQueueObservation = useCallback(() => {
    prioritySnapshotRef.current = null;
    if (observationTimerRef.current !== null) clearTimeout(observationTimerRef.current);
    observationTimerRef.current = null;
    if (observationReadRef.current === seqRef.current) {
      seqRef.current += 1;
      queueReadRef.current?.abort();
    }
    observationReadRef.current = null;
  }, []);

  /** 仅将当前会话的最新成功快照提交到面板和插队反馈。超时作废该次查询的响应。 */
  const load = useCallback(
    async (options?: { timeoutMs?: number }): Promise<void> => {
      const sid = sessionId;
      if (sessionRef.current !== sid) return;
      const seq = ++seqRef.current;
      queueReadRef.current?.abort();
      const controller = new AbortController();
      queueReadRef.current = controller;
      const timeoutMs = options?.timeoutMs;
      let expired = false;
      let timer: ReturnType<typeof setTimeout> | null = null;
      try {
        const raw = fetchCodexQueue(sid, controller.signal);
        let v: CodexQueueView | null;
        if (timeoutMs === undefined) {
          v = await raw;
        } else {
          v = await Promise.race([
            raw,
            new Promise<null>((resolveDeadline) => {
              timer = setTimeout(() => {
                expired = true;
                if (seq === seqRef.current) seqRef.current += 1;
                controller.abort();
                resolveDeadline(null);
              }, timeoutMs);
            }),
          ]);
          if (expired) return;
        }
        // 乱序防护：会话已切换 / 已有更新的请求或操作发出 → 本次响应作废
        if (sessionRef.current !== sid || seq !== seqRef.current) return;
        if (v === null) return; // 403 由全局判废通道处理
        const accepted = acceptedIdsRef.current;
        const fresh = accepted.size
          ? { ...v, actions: v.actions.filter((action) => !accepted.has(action.actionId)) }
          : v;
        setView(fresh);
        prioritySnapshotRef.current?.(fresh);
      } catch {
        // 查询失败时保留上次画面，队列说明只取成功快照。
      } finally {
        if (queueReadRef.current === controller) queueReadRef.current = null;
        if (timer !== null) clearTimeout(timer);
      }
    },
    [sessionId]
  );

  // 挂载/换会话拉取；换会话先清（防串台），并作废在途的去抖定时器与旧响应
  useEffect(() => {
    sessionRef.current = sessionId;
    firstPendingAtRef.current = null;
    acceptedIdsRef.current.clear();
    setView(null);
    setNote(null);
    setForceArmed(false);
    setForceBusy(false);
    setPrioritizeArmedId(null);
    setPrioritizeBusyId(null);
    void load();
    return () => {
      sessionRef.current = ""; // 卸载（key 随会话变化）同样作废旧编辑回执。
      prioritizeOpRef.current += 1; // 在途 prioritize 操作失权：旧 op 无权 setNote/清锁/通知
      seqRef.current += 1; // 在途 load（含有界竞速收尾）响应一律作废
      stopQueueObservation();
      queueReadRef.current?.abort();
      if (debounceRef.current !== null) {
        clearTimeout(debounceRef.current);
        debounceRef.current = null;
      }
    };
  }, [sessionId, load, stopQueueObservation]);

  // R11 + C6：外部触发 → 去抖合并重拉（信号为 0 = 尚无外部触发，不拉）。
  // 去抖带 maxWait 上限：流式 revision 每 ~100ms 推进会不断重置去抖——自首个
  // 未处理触发起超过 QUEUE_REFRESH_MAX_WAIT_MS 即立即重拉（入队条目在明确上限
  // 内可见/可撤，不被流式输出饿死）。
  useEffect(() => {
    if (refreshSignal <= 0) return;
    if (debounceRef.current !== null) clearTimeout(debounceRef.current);
    const now = Date.now();
    if (firstPendingAtRef.current === null) firstPendingAtRef.current = now;
    const elapsed = now - firstPendingAtRef.current;
    const delay = Math.max(
      0,
      Math.min(QUEUE_REFRESH_DEBOUNCE_MS, QUEUE_REFRESH_MAX_WAIT_MS - elapsed)
    );
    debounceRef.current = setTimeout(() => {
      debounceRef.current = null;
      firstPendingAtRef.current = null;
      void load();
    }, delay);
  }, [refreshSignal, load]);

  const edit = async (actionId: string) => {
    if (operationBusy) return;
    const sid = sessionId;
    stopQueueObservation();
    setBusyId(actionId);
    setNote(null);
    try {
      const result = await editCodexQueueMessage(sid, actionId);
      if (sessionRef.current !== sid && !keepEditDraftOnUnmount) return;
      onEditDraft?.({ sessionId: sid, ...result });
    } catch (e) {
      if (sessionRef.current !== sid) return;
      setNote(
        i18n.t("mobile.codex.queue.editFailed", {
          message: e instanceof ApiError ? apiErrorReason(e) : String(e),
        })
      );
    } finally {
      if (sessionRef.current === sid) {
        setBusyId(null);
        await load();
      }
    }
  };

  const retract = useCallback(
    async (actionId: string) => {
      if (operationBusy) return;
      stopQueueObservation();
      setBusyId(actionId);
      setNote(null);
      try {
        await retractCodexAction(actionId);
      } catch (e) {
        if (e instanceof ApiError && e.status === 409) {
          // 已派发不可撤：如实说明（条目按下方重拉的权威状态保留）
          setNote(i18n.t("mobile.codex.queue.retractBusy"));
        } else {
          setNote(
            e instanceof ApiError
              ? i18n.t("mobile.codex.queue.retractFailed", { message: e.message })
              : i18n.t("mobile.codex.queue.retractFailed", { message: String(e) })
          );
        }
      } finally {
        setBusyId(null);
        // 操作后重拉（成功/失败都拉）：以服务端账本为唯一权威呈现
        await load();
      }
    },
    [operationBusy, load, stopQueueObservation]
  );

  // 人工解除无出口屏障（确认后调用）：逐个解除在册屏障并重拉。单个 id 的确定性
  // 「无需解除」回执（409 no_barrier / barrier_in_flight——视图与确认之间屏障被
  // 自动路径清除、该行派发在飞有自动路径等）只收尾该 id，**不中断**剩余 id
  // （2026-10-19 复审 Minor：一败全停修复）；仅真失败（网络/5xx/403 等）→
  // 显示错误不静默（已成功的保留——重拉以服务端账本为权威）。
  const forceContinue = useCallback(async () => {
    if (operationBusy) return;
    const ids = view?.blockedActionIds ?? [];
    setForceArmed(false);
    if (ids.length === 0) return;
    stopQueueObservation();
    setForceBusy(true);
    setNote(null);
    try {
      for (const id of ids) {
        try {
          await forceContinueCodexAction(id);
        } catch (e) {
          if (!isPerIdSettledRefusal(e)) throw e; // 真失败：中断，剩余未解除
        }
      }
    } catch (e) {
      const message = e instanceof ApiError ? apiErrorReason(e) : String(e);
      setNote(i18n.t("mobile.codex.queue.forceContinueFailed", { message }));
    } finally {
      setForceBusy(false);
      await load();
    }
  }, [operationBusy, view, load, stopQueueObservation]);

  // S5：恢复被打断暂停的队列（唯一解除出口；幂等——未暂停同样 200 paused:false）。
  // 成功/失败都重拉：暂停态以服务端为唯一权威。
  const resumeQueue = useCallback(async () => {
    if (operationBusy) return;
    stopQueueObservation();
    setResumeBusy(true);
    setNote(null);
    try {
      await resumeCodexQueue(sessionId);
    } catch (e) {
      const message = e instanceof ApiError ? apiErrorReason(e) : String(e);
      setNote(i18n.t("mobile.codex.queue.resumeFailed", { message }));
    } finally {
      setResumeBusy(false);
      await load();
      // F8：暂停解除 → 同页 composer 的 send-info（queuePaused）已过期，通知重拉
      onQueueStateChanged?.();
    }
  }, [operationBusy, sessionId, load, onQueueStateChanged, stopQueueObservation]);

  const observeQueue = useCallback(
    (isCurrent: () => boolean) => {
      const deadline = Date.now() + QUEUE_OBSERVE_WINDOW_MS;
      const schedule = () => {
        const remaining = deadline - Date.now();
        if (!isCurrent() || remaining <= QUEUE_OBSERVE_INTERVAL_MS) return;
        observationTimerRef.current = setTimeout(async () => {
          observationTimerRef.current = null;
          if (!isCurrent()) return;
          const timeoutMs = Math.min(QUEUE_OP_REFRESH_TIMEOUT_MS, deadline - Date.now());
          if (timeoutMs <= 0) return;
          const seq = seqRef.current + 1;
          observationReadRef.current = seq;
          await load({ timeoutMs });
          if (observationReadRef.current !== seq) return;
          observationReadRef.current = null;
          schedule();
        }, QUEUE_OBSERVE_INTERVAL_MS);
      };
      schedule();
    },
    [load]
  );

  // T06：已入队消息的显式优先处理（POST /codex/queue/prioritize，两步确认）——
  // 「插队」越队派发的是**原 queued 行**（原 actionId/原文/原 cmid；后端
  // 不接受正文副本）；旧队列保持暂停、不重排 FIFO。桌面回合运行中（turnRunning）
  // 先确认停止该回合：expectedTurnId 在**确认点击时刻**冻结（与 T05 stop 同纪律），
  // 随请求发出；暂停空闲仅确认本条越队（confirmInterrupt=false——服务端在空闲时
  // 也不会发任何停止帧）。回执四态如实展示且**关联原 actionId**，不清除其他消息
  // 的行；本入口只对单条显式触发，不提供/不暗示 FIFO 拖动排序。
  const prioritize = useCallback(
    async (actionId: string) => {
      if (operationBusy) return;
      const armedId = prioritizeArmedId;
      setPrioritizeArmedId(null);
      if (armedId !== actionId) return; // 确认臂与执行按钮必须同一条
      // 停止确认按**真实运行态**判定（2026-10-31 用户裁决：插队 = 停止进程后直接
      // 把这条打出去）。空闲时没有进程要停，不得携带停止确认——无论队列是否暂停。
      // 有共享控制器用它冻结的运行态；没有则保守取 false（服务端 Clear 臂会跳过
      // 停止、Busy 臂需要 `interrupt_confirmation_required` 时如实拒绝，不会误停）。
      const turnRunning = turnControl?.turnRunning === true;
      const confirmInterrupt = turnControl ? turnRunning : false;
      const frozenTurnId = turnControl?.activeTurnId ?? undefined;
      // 有效 armed 确认：递增操作代际（旧 op 全部失权），并作废开始前仍在途的
      // 旧刷新快照，再上锁、清 note。
      stopQueueObservation();
      const op = ++prioritizeOpRef.current;
      ++seqRef.current;
      const sid = sessionId;
      const isCurrent = () => sessionRef.current === sid && prioritizeOpRef.current === op;
      setPrioritizeBusyId(actionId);
      setNote(null);
      // note 在其后**所有**到达尾部的路径上都被穷尽分支赋值（try/catch 各自的
      // 归属守卫失权时直接 return），故不做无用初值赋值（no-useless-assignment）。
      let note: string | null;
      try {
        const res = await prioritizeCodexQueueMessage({
          sessionId: sid,
          actionId,
          expectedTurnId: confirmInterrupt ? (frozenTurnId ?? undefined) : undefined,
          confirmInterrupt,
        });
        if (!isCurrent()) return;
        const matches = res.actionId === actionId;
        const accepted =
          matches &&
          ((res.result === "prioritized" &&
            res.receipt.actionId === actionId &&
            res.receipt.delivery === "accepted") ||
            (res.result === "already_dispatched" &&
              res.action.actionId === actionId &&
              res.action.state === "accepted"));
        if (accepted) {
          acceptedIdsRef.current.add(actionId);
          setView(
            (current) =>
              current && {
                ...current,
                actions: current.actions.filter((a) => a.actionId !== actionId),
              }
          );
        }

        const queuedRefusal = res.result === "not_sent" && res.queued;
        const feedback = { snapshot: null as CodexQueueView | null, ready: false };
        const copy = (snapshot: CodexQueueView | null) => {
          if (
            !matches ||
            (res.result === "prioritized" && !accepted) ||
            (res.result === "already_dispatched" && res.action.actionId !== actionId)
          ) {
            return i18n.t("mobile.codex.queue.prioritizeResultUncertain");
          }
          if (queuedRefusal) {
            if (snapshot && !queueFactFromSnapshot(snapshot, actionId)) return null;
            if (snapshot === null) {
              return i18n.t("mobile.codex.queue.prioritizeFailed", {
                message: prioritizeRefusalCopy(res.reasonCode, true),
              });
            }
          }
          if (res.result === "unknown") {
            return [
              prioritizeUnknownPhaseCopy(res.reasonCode),
              queueFactFromSnapshot(snapshot, actionId),
              i18n.t("mobile.codex.queue.prioritizeUnknownGuide"),
            ]
              .filter(Boolean)
              .join(" ");
          }
          return prioritizeResultCopy(res);
        };
        if (matches && (queuedRefusal || res.result === "unknown")) {
          prioritySnapshotRef.current = (snapshot) => {
            feedback.snapshot = snapshot;
            if (!feedback.ready) return;
            const updated = copy(snapshot);
            setNote(updated);
            if (updated === null) prioritySnapshotRef.current = null;
          };
        }

        await load({ timeoutMs: QUEUE_OP_REFRESH_TIMEOUT_MS });
        if (!isCurrent()) return;
        feedback.ready = true;
        note = copy(feedback.snapshot);
        if (note === null) prioritySnapshotRef.current = null;
        // 自动 FIFO 可在插队被拒或派发结果未知后接手。短窗只观察队列，仍可见后续回队。
        if (
          matches &&
          ((queuedRefusal &&
            ["busy_before_write", "barrier_outstanding", "dispatch_in_flight"].includes(
              res.reasonCode
            )) ||
            (res.result === "unknown" && !res.reasonCode.startsWith("stop_")))
        ) {
          observeQueue(isCurrent);
        }
      } catch (e) {
        // catch 入口同样先验归属：旧 op 不得写 note/发旧 scope 收尾 GET。
        if (!isCurrent()) return;
        // HTTP/网络错误按状态/码保守可读化（不裸码、不宣称队列状态、不建议直接
        // 重发，指引先回桌面核对实际状态）；wire 的 status/code 由 ApiError 原样
        // 保留（诊断不丢）。
        note = i18n.t("mobile.codex.queue.prioritizeFailed", {
          message: e instanceof ApiError ? prioritizeHttpErrorCopy(e) : String(e),
        });
        // 错误臂唯一收尾 GET（有界预算；load 自身归 null，不因 GET 失败/超时
        // 进入另一条补确认 GET 的路径）。
        await load({ timeoutMs: QUEUE_OP_REFRESH_TIMEOUT_MS });
        if (!isCurrent()) return;
      }
      setPrioritizeBusyId(null);
      // 停止受理会置暂停/改变 send-info 可用性 → 通知上层（与打断臂同口径）
      onQueueStateChanged?.();
      if (note !== null) setNote(note);
    },
    [
      operationBusy,
      prioritizeArmedId,
      turnControl,
      sessionId,
      load,
      onQueueStateChanged,
      stopQueueObservation,
      observeQueue,
    ]
  );

  if (view === null) return null;
  // 零条待发时，普通运行态由发送区显示；只保留需人工核对或解除的屏障。
  const needsManualResume = view.queuePaused === true && !stopPauseAutoResumes;
  const emptyHidden =
    !needsManualResume &&
    view.actions.length === 0 &&
    (view.blockedReasonCode === null ||
      view.blockedReasonCode === "native_turn_active" ||
      view.blockedReasonCode === "native_turn_pending");
  // 操作收尾的反馈（Unknown 核对提示 / HTTP·网络保守错误）不随空视图被吞：
  // 收尾 GET [] 把队列清空时，复用同一 note 反馈区域最小呈现（未操作空队列
  // note 为空仍按原自隐，不加壳不加新入口）。
  if (emptyHidden) {
    if (note === null) return null;
    return (
      <div
        data-testid="codex-queue-panel"
        className="rounded-xl border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2"
      >
        <p
          data-testid="codex-queue-note"
          className="mt-1 text-xs text-slate-600 first:mt-0 dark:text-slate-300"
        >
          {note}
        </p>
      </div>
    );
  }
  // 人工解除按钮仅两类**无自动恢复路径**的阻塞态可见（native_turn_pending 等待
  // Turn 终态自动继续，不提供）；靶向清单来自服务端 blockedActionIds
  const canForceContinue =
    (view.blockedReasonCode === "native_delivery_unknown" ||
      view.blockedReasonCode === "compat_outstanding") &&
    (view.blockedActionIds?.length ?? 0) > 0;
  // T05：共享停止控制器的在飞闸（composer 停止入口在飞时，本面板打断臂同样锁住
  // ——同一破坏性动作不出现第二条并发指令）
  const stopGated = turnControl?.stopping === true;
  // 阻塞文案分流（分码 + 能力位）：有打断能力的 build 才承诺「可点下方停止」，
  // 无能力 build 如实说明暂无停止出口（按钮在源头隐藏，文案也不撒谎）
  const blockedBannerKey = () => {
    if (view.blockedReasonCode === "native_turn_pending")
      return view.canInterrupt === true
        ? "mobile.codex.queue.blockedNativeTurn"
        : "mobile.codex.queue.blockedNativeTurnNoCap";
    if (view.blockedReasonCode === "native_turn_active")
      return view.canInterrupt === true
        ? "mobile.codex.queue.blockedNativeTurnActive"
        : "mobile.codex.queue.blockedNativeTurnActiveNoCap";
    if (view.blockedReasonCode === "native_delivery_unknown")
      return "mobile.codex.queue.blockedNativeUnknown";
    return "mobile.codex.queue.blockedCompat";
  };

  // C04（队列展示收纳）：折叠摘要的状态词。C05（复审 Q02）：状态词按**阻塞原因**
  // 分流，不以 blockedActionIds 非空判断——该清单同时包含「已受理并绑定回合」的
  // 正常屏障（native_turn_pending，回合终态会自动释放），把它说成「待人工核对」
  // 指向一个根本不存在的解除动作。同理，清单读取降级为空不能反证无需核对：阻塞
  // 码由另一路独立派生，声称 Unknown/兼容时仍如实提示；两者并存时核对原因优先。
  const queuedCount = view.actions.filter((a) => a.state === "queued").length;
  const needsManualVerify =
    view.blockedReasonCode === "native_delivery_unknown" ||
    view.blockedReasonCode === "compat_outstanding";
  // 有自动恢复路径的阻塞态：回合完成后屏障自动释放、队列自动接力
  const awaitingTurn =
    !needsManualVerify &&
    (view.blockedReasonCode === "native_turn_pending" ||
      view.blockedReasonCode === "native_turn_active");
  const summaryStatusKey = needsManualVerify
    ? "mobile.codex.queue.summaryVerify"
    : stopPauseAutoResumes
      ? "mobile.codex.queue.summaryStopPause"
      : awaitingTurn
        ? "mobile.codex.queue.summaryAwaitTurn"
        : view.queuePaused === true
          ? "mobile.codex.queue.summaryPaused"
          : queuedCount > 0
            ? "mobile.codex.queue.summaryQueued"
            : "mobile.codex.queue.summaryIdle";

  return (
    <div
      data-testid="codex-queue-panel"
      className="rounded-xl border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2"
    >
      {/* 收起时仍显示摘要，包括待人工核对状态。新消息入队时刷新计数。
          data-* 供测试做语言无关断言。 */}
      <div className="flex flex-wrap items-center gap-2">
        <span
          data-testid="codex-queue-summary"
          data-count={queuedCount}
          data-paused={view.queuePaused === true || undefined}
          data-verify-pending={needsManualVerify || undefined}
          data-awaiting-turn={awaitingTurn || undefined}
          className="text-xs text-[var(--mut)]"
        >
          {i18n.t("mobile.codex.queue.summaryCount", { n: queuedCount })} ·{" "}
          {i18n.t(summaryStatusKey)}
        </span>
        <button
          type="button"
          data-testid="codex-queue-toggle"
          aria-expanded={expanded}
          onClick={() => setExpanded((v) => !v)}
          className="ml-auto rounded-full border border-[var(--bd)] px-2 py-0.5 text-xs text-[var(--mut)] underline"
        >
          {i18n.t(expanded ? "mobile.codex.queue.summaryHide" : "mobile.codex.queue.summaryView")}
        </button>
      </div>
      {expanded && (
        <>
          {needsManualVerify && (
            <p
              data-testid="codex-queue-blocked"
              data-reason={view.blockedReasonCode}
              className="mt-1 text-xs text-amber-700 dark:text-amber-400"
            >
              {i18n.t(blockedBannerKey())}
            </p>
          )}
          {/* 停止产生的临时冻结会随目标终态解除，其他暂停需要手动恢复。 */}
          {view.queuePaused === true &&
            (needsManualResume ||
              view.actions.length > 0 ||
              (view.blockedActionIds?.length ?? 0) > 0) && (
              <div data-testid="codex-queue-paused-block" className="mt-1">
                <p
                  data-testid="codex-queue-paused"
                  className="text-xs text-amber-700 dark:text-amber-400"
                >
                  {i18n.t(
                    stopPauseAutoResumes
                      ? "mobile.codex.queue.stopPauseNote"
                      : "mobile.codex.queue.pausedNote"
                  )}
                </p>
                {(!stopPauseAutoResumes || manualStopPauseRecovery) && (
                  <button
                    type="button"
                    data-testid="codex-queue-resume"
                    disabled={operationBusy}
                    onClick={() => {
                      void resumeQueue();
                    }}
                    className="mt-1 rounded-full bg-amber-500/20 px-2 py-0.5 text-xs font-semibold text-amber-700 disabled:opacity-40 dark:bg-amber-400/20 dark:text-amber-300"
                  >
                    {i18n.t("mobile.codex.queue.resume")}
                  </button>
                )}
              </div>
            )}
          {/* 人工解除（无出口屏障复审 Important）：两步确认——未确认不发任何请求；
          文案如实说明风险（消息可能实际已投出，继续后续消息可能造成重复内容）。
          人工解除是显式覆盖，「无自动恢复路径」的说明语义不变。 */}
          {canForceContinue && !forceArmed && (
            <button
              type="button"
              data-testid="codex-queue-force-continue"
              disabled={operationBusy}
              onClick={() => {
                setNote(null);
                setForceArmed(true);
              }}
              className="mt-1 rounded-full border border-amber-500/50 px-2 py-0.5 text-xs text-amber-700 disabled:opacity-40 dark:text-amber-300"
            >
              {i18n.t("mobile.codex.queue.forceContinue")}
            </button>
          )}
          {canForceContinue && forceArmed && (
            <span
              data-testid="codex-queue-force-confirm"
              className="mt-1 flex w-full flex-wrap items-center gap-2 rounded-lg bg-amber-500/10 px-2 py-1 text-[11px] text-amber-700 dark:text-amber-300"
            >
              <span>{i18n.t("mobile.codex.queue.forceContinueRisk")}</span>
              <button
                type="button"
                data-testid="codex-queue-force-yes"
                disabled={operationBusy}
                onClick={() => {
                  void forceContinue();
                }}
                className="rounded-full bg-amber-600 px-2 py-0.5 font-semibold text-white disabled:opacity-40 dark:bg-amber-500"
              >
                {i18n.t("mobile.codex.queue.forceContinueYes")}
              </button>
              <button
                type="button"
                data-testid="codex-queue-force-no"
                disabled={operationBusy}
                onClick={() => {
                  setForceArmed(false);
                }}
                className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-[var(--tx)] disabled:opacity-40"
              >
                {i18n.t("mobile.codex.queue.forceContinueCancel")}
              </button>
            </span>
          )}
          <ul className="mt-1 space-y-1">
            {view.actions.map((a) => (
              <li
                key={a.actionId}
                data-testid={`codex-queue-row-${a.actionId}`}
                className="flex flex-nowrap items-center gap-1.5 rounded-lg bg-[var(--btnp)]/5 px-2 py-1 text-xs"
              >
                <span
                  data-testid={`codex-queue-position-${a.actionId}`}
                  className="shrink-0 text-amber-700 dark:text-amber-300"
                >
                  {/* queued 且有位次 → 「第N位」（FIFO 真序）；否则按状态词呈现
                  （cancelled/unknown 等终态如实留痕，无重试/插队入口） */}
                  {a.state === "queued" && a.position !== null && a.position >= 1
                    ? i18n.t("mobile.codex.queue.position", { n: a.position })
                    : stateLabel(a.state)}
                </span>
                {/* P4 + P6：正文预览。P6（2026-10-31 用户要求）起与位次/长度/按钮
                同排一行：`min-w-0 flex-1 truncate` 占据剩余空间并在超宽时省略，
                按钮 `shrink-0` 永不被挤出屏幕。旧后端缺省 / accepted/unknown 行
                为 undefined → 不渲染（前端不猜）。*/}
                {typeof a.textPreview === "string" && a.textPreview !== "" && (
                  <>
                    <span
                      data-testid={`codex-queue-preview-${a.actionId}`}
                      className="min-w-0 flex-1 truncate text-[var(--mut)]"
                    >
                      {queueRowPreview(a.textPreview)}
                    </span>
                    <span
                      data-testid={`codex-queue-length-${a.actionId}`}
                      className="shrink-0 text-[10px] text-[var(--mut)] tabular-nums opacity-70"
                    >
                      {i18n.t("mobile.codex.queue.lengthChars", {
                        n: queueRowLength(a.textPreview),
                      })}
                    </span>
                  </>
                )}
                {/* T06：插队（仅 queued 行；两步确认——运行中先确认停止该回合，
                暂停空闲仅确认本条越队；共享停止入口在飞时同样锁住）。这不是
                FIFO 拖动排序：只对单条显式触发，越队授权由服务端按原 action
                一次性发放，其余行位次原样。 */}
                {a.state === "queued" && (
                  <button
                    type="button"
                    data-testid={`codex-queue-prioritize-${a.actionId}`}
                    disabled={operationBusy || stopGated}
                    onClick={() => {
                      setNote(null);
                      setPrioritizeArmedId(a.actionId);
                    }}
                    className="ml-auto rounded-full border border-emerald-500/50 px-2 py-0.5 text-xs text-emerald-800 disabled:opacity-40 dark:text-emerald-300"
                  >
                    {i18n.t("mobile.codex.queue.prioritize")}
                  </button>
                )}
                {a.state === "queued" && onEditDraft && (
                  <button
                    type="button"
                    data-testid={`codex-queue-edit-${a.actionId}`}
                    disabled={operationBusy}
                    onClick={() => {
                      void edit(a.actionId);
                    }}
                    className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--mut)] disabled:opacity-40"
                  >
                    {i18n.t("mobile.codex.queue.edit")}
                  </button>
                )}
                {/* 撤回：仅 queued 可撤；**没有通用插队/重试入口**（注释见文件头） */}
                {a.state === "queued" && (
                  <button
                    type="button"
                    data-testid={`codex-queue-retract-${a.actionId}`}
                    disabled={operationBusy}
                    onClick={() => {
                      void retract(a.actionId);
                    }}
                    className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--mut)] disabled:opacity-40"
                  >
                    {i18n.t("mobile.codex.queue.retract")}
                  </button>
                )}
              </li>
            ))}
          </ul>
          {/* T06：优先处理确认臂（两步确认；文案按运行态分流——运行中披露停止风险，
          暂停空闲只确认越队；未确认不发任何请求） */}
          {(() => {
            const armedRow = prioritizeArmedId
              ? view.actions.find((a) => a.actionId === prioritizeArmedId && a.state === "queued")
              : null;
            if (armedRow == null) return null;
            const stopRisk = turnControl?.turnRunning === true;
            return (
              <span
                data-testid={`codex-queue-prioritize-confirm-${armedRow.actionId}`}
                className="mt-1 flex w-full flex-wrap items-center gap-2 rounded-lg bg-emerald-500/10 px-2 py-1 text-[11px] text-emerald-800 dark:text-emerald-300"
              >
                <span>
                  {stopRisk
                    ? i18n.t("mobile.codex.queue.prioritizeRiskStop")
                    : i18n.t("mobile.codex.queue.prioritizeRiskBypass")}
                </span>
                <button
                  type="button"
                  data-testid={`codex-queue-prioritize-yes-${armedRow.actionId}`}
                  disabled={operationBusy || stopGated}
                  onClick={() => {
                    void prioritize(armedRow.actionId);
                  }}
                  className="rounded-full bg-emerald-600 px-2 py-0.5 font-semibold text-white disabled:opacity-40 dark:bg-emerald-500"
                >
                  {i18n.t("mobile.codex.queue.prioritizeYes")}
                </button>
                <button
                  type="button"
                  data-testid={`codex-queue-prioritize-no-${armedRow.actionId}`}
                  disabled={prioritizeBusyId !== null}
                  onClick={() => {
                    setPrioritizeArmedId(null);
                  }}
                  className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-[var(--tx)] disabled:opacity-40"
                >
                  {i18n.t("mobile.codex.queue.prioritizeCancel")}
                </button>
              </span>
            );
          })()}
          {note !== null && (
            <p
              data-testid="codex-queue-note"
              className="mt-1 text-xs text-slate-600 dark:text-slate-300"
            >
              {note}
            </p>
          )}
        </>
      )}
    </div>
  );
}

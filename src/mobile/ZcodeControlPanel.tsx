// ZCode 统一控制面板（T7.2 Slice B1a）：手机端最小发送面。
//
// 纪律（与 T7.2 计划 / useZcodeControl 对齐）：
// - 连接态只来自 hook 的官方 GET；未确认一律 `unknown`，已保存链接绝不当成已连接
//   （文案里也不出现「已连接 / 已配对」这类断言性措辞）。
// - 发送只走官方 tagged send（hook.submit → POST /m/api/v1/zcode/actions），
//   **不**回退旧 `session-send`、无头 CLI 或终端按键。
// - 回执如实：accepted=已受理（≠ 已生效），confirmed 才显示已生效，unknown 保持中性；
//   send_failed/rejected/unknown 保留草稿、不自动重发。就地原因只展示本地化文案，
//   `data-reason-code` 才是冻结错误码本身。
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ClipboardEvent as ReactClipboardEvent,
} from "react";
import i18n from "@/i18n";
import {
  isZcodeErrorCode,
  enqueueZcodeMessage,
  editZcodeQueuedMessage,
  getZcodeQueueRecovery,
  acknowledgeZcodeQueueRecovery,
  getZcodeEditSnapshot,
  prioritizeZcodeMessage,
  resumeZcodeLocalQueue,
  retractZcodeQueuedMessage,
  type CollaborationMode,
  type EditResult,
  type QueueRecoveryView,
  type ActionReceipt,
  type ConnectionView,
  type ControlAction,
  type HeldQueueChoice,
  type HeldQueueDisposition,
  type QueueView,
  type SessionView,
  type ZcodeErrorCode,
  ZcodeControlApiError,
} from "@/lib/api/zcodeControl";
import useZcodeControl from "./useZcodeControl";
import ZcodeLocalQueuePanel from "./ZcodeLocalQueuePanel";
import { zcodeQueuePreview } from "./zcode-local-queue-state";
import { resolveModelChoice } from "./zcode-selection-state";
import { newActionId } from "./codexIpcApi";
import ZcodePendingCard, { STATUS_KEYS, pendingDomId, zcodeReasonText } from "./ZcodePendingCard";
import ZcodeQueuePanel, { dispatchWritable } from "./ZcodeQueuePanel";
import { deriveZcodePrimaryAction } from "./zcode-primary-action";
import {
  attachmentMarkup,
  isImageFile,
  newAttachmentId,
  type PendingAttachment,
} from "./attachments";
import { ApiError, fetchZcodeControl, postZcodeTakeover, uploadAttachment } from "./api";

interface ZcodeControlPanelProps {
  /** 会话原 ID（服务端按它绑定 ZCode 会话；手机不自报 workspace） */
  sessionId: string;
}

/** 附件存储说明段落的 DOM id（aria-controls 的锚点；面板内唯一）。 */
const ATTACH_HINT_ID = "zcode-attach-hint";

/**
 * 队列事务的「新鲜证据」预算（计划 §5.3）：`accepted` 只是**已受理**，撤回是否真的
 * 生效只能靠随后的权威 GET 判定。4s 内拿不到新鲜投影 → `uncertain`（不回填、不重发），
 * 这是刻意选的「宁可让用户人工核对，也不把可能已执行的消息再编辑一遍」。
 */
const QUEUE_CONFIRM_TIMEOUT_MS = 4000;
const QUEUE_CONFIRM_POLL_MS = 200;

/** 队列动作的可见结果（文案按 state 取键，错误态另挂冻结码）。 */
type QueueNotice =
  | { state: "not-queued"; original?: string }
  | { state: "stale" }
  | { state: "unchanged" }
  | { state: "uncertain"; original?: string }
  | { state: "error"; reason: string };

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** 传输/回执异常 → 冻结原因码（与 hook 同口径：网络层 offline，其余 unknown）。 */
function queueErrorCode(error: unknown): ZcodeErrorCode | "unknown" {
  if (error instanceof ZcodeControlApiError) {
    return error.code ?? (error.status === null ? "offline" : "unknown");
  }
  return "unknown";
}

/**
 * 冻结原因码 → 本地化文案键。词表随待处理卡一起下沉到
 * [`ZcodePendingCard`](./ZcodePendingCard.tsx)（面板消费该组件，词表留在面板会形成
 * 循环 import）；此处按原名 re-export，既有导入方与测试不受影响。
 */
export { ZCODE_REASON_KEYS, zcodeReasonText } from "./ZcodePendingCard";

/**
 * 写面硬门：`connection.state === "paired"` **且** `session.ready === true` 才放行。
 * 返回 null = 可写；否则返回冻结原因码（陈旧投影/未配对连接一律零写）。
 * `connecting` 等配对前态不在冻结码表内 → 统一按 `not_ready` 呈现。
 */
function actionGateReason(
  connection: ConnectionView | null,
  session: SessionView | null
): string | null {
  if (connection === null) return "unknown";
  if (connection.state !== "paired") {
    if (isZcodeErrorCode(connection.reasonCode)) return connection.reasonCode;
    if (isZcodeErrorCode(connection.state)) return connection.state;
    return "not_ready";
  }
  if (session === null || !session.ready) return "not_ready";
  return null;
}

export default function ZcodeControlPanel({ sessionId }: ZcodeControlPanelProps) {
  const {
    connection,
    session,
    localQueue,
    reason,
    submitting,
    draftCleared,
    submit,
    refresh,
    refreshLocalQueue,
  } = useZcodeControl(sessionId);
  const [draft, setDraft] = useState("");
  /**
   * `inputRouting.mode="choice"` + 非空队列：发送前必须让用户显式选择旧队列处置
   * （保留/清空/取消）。默认焦点在「保留」，清空是破坏性操作绝不默认。
   */
  const [heldQueuePrompt, setHeldQueuePrompt] = useState(false);
  const [heldQueueChanged, setHeldQueueChanged] = useState(false);
  const heldQueueKeepRef = useRef<HTMLButtonElement | null>(null);
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);
  /** 组件是否仍挂载：探测在途时卸载 ⇒ 立即停止（不再 setState、不再发 GET）。 */
  const aliveRef = useRef(true);
  useEffect(() => {
    aliveRef.current = true;
    return () => {
      aliveRef.current = false;
    };
  }, []);
  /** 已保存的官方链接（只读 GET /m/api/v1/official-control；读不到 → null = 不挂入口）。 */
  const [officialUrl, setOfficialUrl] = useState<string | null>(null);
  /** 同帧双击闸：一次用户动作只允许一个在途 POST（双击不产生第二个 actionId）。 */
  const inFlightRef = useRef(false);
  /** 最近一次 send 提交的原文：accepted 迟到时只清这份，不碰回执到达前续写的草稿。 */
  const submittedDraftRef = useRef<string | null>(null);
  /** 草稿即时值（effect 之外也要读最新值，避免闭包拿到旧草稿）。 */
  const draftRef = useRef(draft);
  // 三轮 C：附件态（与 MessageComposer 同一上传管线/条目形状/引用约定）。
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  // 渲染期写 ref 被 react-hooks 规则禁止：同步镜像改在 effect（发送读 refs 语义不变）。
  const attachmentsRef = useRef<PendingAttachment[]>([]);
  useEffect(() => {
    attachmentsRef.current = attachments;
  }, [attachments]);
  /** 上传中条目的取消柄（× 移除 = XHR abort，防白传到底）。 */
  const uploadCtrlsRef = useRef(new Map<string, AbortController>());
  /** 本次 send 提交的就绪附件 id：accepted 清的是「随本次发送」的条目，不碰续加。 */
  const submittedAttachmentIdsRef = useRef<string[] | null>(null);
  /**
   * 附件存储说明的展开态（R4/U1）：默认折叠，点「？」展开既有说明、再点收起。
   * 纯展示开关：绝不触发文件选择器 / 上传 / 发送。
   */
  const [attachHintOpen, setAttachHintOpen] = useState(false);
  const attachInputRef = useRef<HTMLInputElement | null>(null);
  /**
   * 当前展示的待处理卡（多组并存时的切组状态）。存 id 而不是下标：宿主增删卡
   * 不会把选择挪到别的请求上；id 一旦消失则回落到首张（绝不空屏丢卡）。
   */
  const [pendingTabId, setPendingTabId] = useState<string | null>(null);

  // ==== W4：队列动作的共享在途槽与草稿事务 ====
  /** 当前在途的队列项（单写槽：与 `inFlightRef` 同源，一次只允许一个写命令）。 */
  const [queueBusyItemId, setQueueBusyItemId] = useState<string | null>(null);
  /** 「恢复运行」在途。 */
  const [queueBusyResume, setQueueBusyResume] = useState(false);
  /** 本地待发队列的入队/单条编排在途（与主按钮共用单写槽）。 */
  const [localQueueBusy, setLocalQueueBusy] = useState(false);
  const [localQueueNotice, setLocalQueueNotice] = useState<string | null>(null);
  const [localQueueError, setLocalQueueError] = useState<ZcodeErrorCode | "unknown" | null>(null);
  const [priorityPrompt, setPriorityPrompt] = useState<{ itemId: string; version: number } | null>(
    null
  );
  const [editConflict, setEditConflict] = useState<{
    requestId: string;
    result: EditResult;
  } | null>(null);
  const [editPreflight, setEditPreflight] = useState<{
    itemId: string;
    version: number;
  } | null>(null);
  const [queueRecovery, setQueueRecovery] = useState<QueueRecoveryView | null>(null);
  /** 最近一次“再编辑”的请求 ID：新入队时带给服务端消费快照。 */
  const editedFromRef = useRef<string | null>(null);
  /** 网络未知的入队请求：同草稿/同附件重试必须复用原 requestId，绝不换 ID 再登记一份。 */
  const lastEnqueueRef = useRef<{
    requestId: string;
    text: string;
    paths: string[];
  } | null>(null);
  /** 队列动作的可见结果（撤回完成/结果不确定/失败原因）。 */
  const [queueNotice, setQueueNotice] = useState<QueueNotice | null>(null);
  /** 只有**新鲜**队列 `paused=false` 才为 true（此前绝不说「已恢复」）。 */
  const [resumeConfirmed, setResumeConfirmed] = useState(false);
  /** 剪贴板不可用时的原文兜底（只读文本框，供人工复制）。 */
  const [copyFallback, setCopyFallback] = useState<string | null>(null);
  const [copyDone, setCopyDone] = useState(false);
  const draftInputRef = useRef<HTMLTextAreaElement | null>(null);
  /** 最近一次落地的会话投影（effect 同步）：队列事务要读**最新**代际，不吃渲染闭包。 */
  const sessionRef = useRef<SessionView | null>(null);
  useEffect(() => {
    sessionRef.current = session;
  }, [session]);

  /**
   * 草稿的**唯一**写入口（计划 §5.6）：手动输入、发送后清空、切会话共用，
   * 保证 `draftRef.current` 与 state 永远同值，消除两处各写一半的竞态。
   */
  const updateDraft = useCallback((text: string) => {
    draftRef.current = text;
    setDraft(text);
  }, []);

  // 「已恢复」只对**本次**恢复有效：队列再次被暂停（回合停止/出错）→ 立即收回这句话，
  // 免得「已暂停」和「已恢复」并排出现。
  useEffect(() => {
    if (session?.queue?.paused === true) setResumeConfirmed(false);
  }, [session?.queue?.paused]);

  // 仅 send 的 accepted 允许清本次草稿（hook 语义）；失败/拒绝/未知一律保留。
  // 迟到回执竞态：accepted 到达时若草稿已被续写成别的内容，那是**新**输入，必须保留——
  // 只有草稿仍等于本次提交原文才清空（连点两次发送的场景由 hook 复位标记兜底）。
  useEffect(() => {
    if (!draftCleared) return;
    const submitted = submittedDraftRef.current;
    // 竞态守卫：草稿被续写 → 那是新输入，草稿保留。
    const unchanged = submitted !== null && draftRef.current === submitted;
    if (unchanged) {
      updateDraft("");
    }
    // 三轮评审 P2-2：附件清理**独立于草稿**——已上传并引用进已发送正文的附件属
    // 本次提交，accepted 即按提交批次 id 清除（续加的新附件保留）；文字才按是否
    // 续写决定去留。否则续写期间 accepted 会把旧附件留到下一次发送重复携带。
    const sentIds = submittedAttachmentIdsRef.current;
    if (sentIds !== null) {
      submittedAttachmentIdsRef.current = null;
      setAttachments((prev) => prev.filter((a) => !sentIds.includes(a.id)));
    }
  }, [draftCleared, updateDraft]);

  /** choice 选择器打开时默认焦点在「保留旧队列」（破坏性选项绝不默认聚焦）。 */
  useEffect(() => {
    if (heldQueuePrompt) heldQueueKeepRef.current?.focus();
  }, [heldQueuePrompt]);

  // 换会话清旧回执与旧草稿：上一次会话的受理状态不得贴到新会话。
  useEffect(() => {
    setReceipt(null);
    updateDraft("");
    submittedDraftRef.current = null;
    // 切会话：旧会话的 choice 选择器不得留下（期望 IDs 属于旧队列）。
    setHeldQueuePrompt(false);
    setHeldQueueChanged(false);
    // W4：队列动作的在途标识/结果/恢复确认/冲突对话/剪贴板兜底都不跨会话。
    setQueueBusyItemId(null);
    setQueueBusyResume(false);
    setQueueNotice(null);
    setResumeConfirmed(false);
    setLocalQueueBusy(false);
    setLocalQueueNotice(null);
    setLocalQueueError(null);
    setPriorityPrompt(null);
    setEditConflict(null);
    setEditPreflight(null);
    setQueueRecovery(null);
    editedFromRef.current = null;
    setCopyFallback(null);
    setCopyDone(false);
    // 三轮 C：附件与在途上传不跨会话残留。
    setAttachments([]);
    submittedAttachmentIdsRef.current = null;
    // 附件说明的展开态同样不跨会话残留（说明属当前会话的附件管线）。
    setAttachHintOpen(false);
    // 切组选择不跨会话残留（旧会话的 interaction id 在新会话没有意义）。
    setPendingTabId(null);
    setPendingProvider(null);
    setPendingModel(null);
    // 抢回面同样不跨会话残留（T8）。
    setTakeoverBusy(false);
    setTakeoverStillConflict(false);
    setTakeoverError(null);
    takeoverInFlightRef.current = false;
    // 三轮评审 P2-1：实际挂载点（SessionDetail）以会话 id 为 **key**——切会话 =
    // 卸载整个组件，本 effect 体不会执行；卸载必须与换 sessionId 同语义。cleanup
    // 在卸载与依赖变更两条路径都取消在途上传（防旧会话附件继续传输落盘）。
    return () => {
      for (const ctrl of uploadCtrlsRef.current.values()) ctrl.abort();
      uploadCtrlsRef.current.clear();
    };
  }, [sessionId, updateDraft]);

  // A5 基线「打开官方页面」的显式辅助入口：只读已保存链接（不自动打开、不代理点击）。
  useEffect(() => {
    let alive = true;
    setOfficialUrl(null);
    void fetchZcodeControl().then((availability) => {
      if (alive) setOfficialUrl(availability.url);
    });
    return () => {
      alive = false;
    };
  }, [sessionId]);

  // ==== T8 冲突态「断开并抢回」 ====
  /** 抢回 POST 在途（busy 态 + 同帧双击闸）。 */
  const [takeoverBusy, setTakeoverBusy] = useState(false);
  /** 服务端一次抢回后仍 conflict：只提示先关官方页面，绝不二次请求。 */
  const [takeoverStillConflict, setTakeoverStillConflict] = useState(false);
  /** 抢回失败的冻结原因码（文案按码本地化，码只进 data-reason-code）。 */
  const [takeoverError, setTakeoverError] = useState<ZcodeErrorCode | "unknown" | null>(null);
  const takeoverInFlightRef = useRef(false);

  /** 一次抢回一个在途槽：busy 期间重入一律丢弃（绝不产生第二次 POST）。 */
  const handleTakeover = () => {
    if (takeoverInFlightRef.current) return;
    takeoverInFlightRef.current = true;
    setTakeoverBusy(true);
    setTakeoverStillConflict(false);
    setTakeoverError(null);
    void postZcodeTakeover()
      .then((view) => {
        // 仍 conflict（官方页还在线）：服务端已按契约只试一次，这里只提示。
        setTakeoverStillConflict(view.state === "conflict");
        // 成功（含仍 conflict）后重拉权威连接/会话视图（现有 GET 纠偏机制）。
        void refresh();
      })
      .catch((error: unknown) => {
        // 失败显示冻结码原因：网络层（status=null）按 offline 口径，其余按服务端码。
        setTakeoverError(
          error instanceof ZcodeControlApiError
            ? (error.code ?? (error.status === null ? "offline" : "unknown"))
            : "unknown"
        );
      })
      .finally(() => {
        takeoverInFlightRef.current = false;
        setTakeoverBusy(false);
      });
  };

  const openOfficial = () => {
    if (officialUrl === null) return;
    window.open(officialUrl, "_blank", "noopener,noreferrer");
  };

  // 写面硬门：连接已配对 + 会话就绪（陈旧投影 ready=false 一律零写）。
  const gateReason = actionGateReason(connection, session);
  // 发送区原因优先取 hook 的如实连接/回执原因；连接正常但投影未就绪时显示 not_ready。
  const displayReason = reason ?? gateReason;

  /**
   * 待处理卡的展示组：默认第一张；用户切过的 id 若仍在册就继续用，被宿主撤掉则回落
   * 首张——其余卡一直挂载（视觉隐藏），所以切组不会丢任何草稿，也绝不丢请求。
   */
  const pendings = session?.pending ?? [];
  const activePendingId =
    pendings.find((pending) => pending.interactionId === pendingTabId)?.interactionId ??
    pendings[0]?.interactionId ??
    null;

  /**
   * 统一写入口（计划 §5.6）：返回**typed 回执**，供队列事务 `await` 判断命运。
   * 单在途槽仍是 `inFlightRef`——第二次调用直接抛（绝不产生第二个 actionId）；
   * 回执落地与清零原因面的语义与原 `runOnce` 完全一致（既有主按钮行为不变）。
   */
  const runAction = useCallback(
    async (action: ControlAction): Promise<ActionReceipt> => {
      if (inFlightRef.current) throw new Error("zcode_action_busy");
      inFlightRef.current = true;
      // 新提交开始即清旧回执（C2）：上一次的「已受理」绝不得贴到在途/失败/未知的
      // 新动作上——accepted 越级正是本面板要防的失真。
      setReceipt(null);
      try {
        const next = await submit(action);
        setReceipt(next);
        return next;
      } finally {
        inFlightRef.current = false;
      }
    },
    [submit]
  );

  /** 一次动作一个在途槽：同帧重入直接丢弃，回执只认第一次提交。 */
  const runOnce = (action: ControlAction) => {
    if (inFlightRef.current) return;
    void runAction(action).catch(() => {
      // 传输层失败：hook 已置冻结原因码；草稿保留，绝不自动重发。
    });
  };

  // ==== R5/U3：发送／打断共用主按钮 ====
  // 打断目标：只认新鲜 control 里唯一的 primaryTurn（多目标拒绝，禁止随取第一个）；
  // 只发 stop，绝不夹带 sendText。
  const stopTargets = (session?.control?.activeWorks ?? []).filter(
    (work) => work.kind === "primaryTurn" && work.foregroundExecutionId !== null
  );
  const stopTarget = stopTargets.length === 1 ? stopTargets[0].foregroundExecutionId : null;
  /** 官方新鲜 control 判定「运行中」（不看看板颜色、不猜本地忙态）。 */
  const running = stopTargets.length > 0;
  /** 运行时/本地已有待发/队列暂停：普通发送先进入 Tuvis 待发，不直接抢上游。 */
  const sendUsesOutbox =
    running ||
    localQueue?.autoDrain === false ||
    (localQueue?.items.some((item) => item.state === "queued") ?? false);
  /**
   * 附件闸：上传中/失败条目在列 → 此刻不可发送（沿用既有提示阻止发送）。附件在途
   * 时主按钮**留在发送位**、绝不翻成打断（用户正在组装消息，不能被静默换成打断）。
   */
  const attachmentBlocked = attachments.some(
    (a) => a.status === "uploading" || a.status === "failed"
  );
  /**
   * 有内容在手：文字非空**或**存在**就绪**附件（就绪 = 上传完成、可随正文上行）。
   * 上传中/失败附件不算可发内容——它们只把发送位锁住并挂既有提示。
   */
  const hasContent = draft.trim().length > 0 || attachments.some((a) => a.status === "ready");
  /**
   * 旧代际：会话投影代际落后于连接代际 → 写面零写（与服务端 stale_generation 同口径：
   * 旧代际的 stop 目标必然失效、服务端也必然拒收）。只关主按钮，逐题卡语义不变（R3）。
   */
  const staleGeneration =
    connection !== null && session !== null && session.generation < connection.generation;
  /** 发送闸：写面硬门 + send 能力 + 附件闸 + 代际新鲜度。 */
  const sendAllowed =
    gateReason === null &&
    session !== null &&
    session.capabilities.send.enabled &&
    !attachmentBlocked &&
    !staleGeneration;
  /** 打断闸：写面硬门 + interrupt 能力 + 唯一 stop 目标（既有守卫原值）+ 代际新鲜度。 */
  const stopAllowed =
    gateReason === null &&
    session !== null &&
    session.capabilities.interrupt.enabled &&
    stopTarget !== null &&
    !attachmentBlocked &&
    !staleGeneration;
  /** 在途（任一动作 POST 未回，**含队列动作**）→ 主按钮禁用：零第二个动作。 */
  const busy = submitting || queueBusyItemId !== null || queueBusyResume || localQueueBusy;
  /** 主按钮唯一动作（纯函数推导，见 zcode-primary-action.ts）。 */
  const primaryAction = deriveZcodePrimaryAction({
    hasContent,
    running,
    sendAllowed,
    stopAllowed,
    busy,
  });
  /**
   * 打断位：运行中 + 手上无可发内容 + 附件闸未闭 → 按钮代表「打断当前回合」。
   * 有内容（含就绪附件）时按钮留在发送位：有内容发送绝不先打断。
   */
  const stopSlot = running && !hasContent && !attachmentBlocked;
  /**
   * 主按钮旁的原因（冻结码）：删除独立打断行后，原因必须仍在按钮旁可见。
   * 优先级：写面硬门 > 旧代际 > 打断位（能力关闭 / 目标歧义）> 发送位（能力关闭）。
   * 附件闸另有既有提示（zcode-attach-notice）说明原因，这里不重复冻结码。
   */
  const primaryReason: string | null =
    primaryAction !== "disabled"
      ? null
      : (gateReason ??
        (staleGeneration
          ? "stale_generation"
          : stopSlot
            ? session !== null && !session.capabilities.interrupt.enabled
              ? (session.capabilities.interrupt.reasonCode ?? "unknown")
              : stopTarget === null
                ? "target_ambiguous"
                : null
            : session !== null && !session.capabilities.send.enabled
              ? (session.capabilities.send.reasonCode ?? "not_ready")
              : null));

  /**
   * 附件闸提示（既有提示，R5 保留）：上传中/失败条目在列 → 提示「等上传完成再发送」
   * /「先移除失败附件」。**派生自附件状态**（不再是点击后的瞬时提示）：主按钮此时
   * 已禁用，用户必须先看到原因才知道为什么点不动。
   */
  const attachNotice: "uploading" | "failed" | null = attachments.some(
    (a) => a.status === "uploading"
  )
    ? "uploading"
    : attachments.some((a) => a.status === "failed")
      ? "failed"
      : null;

  // 三轮 C：附件上传（通用通道；状态机与 MessageComposer 同形，无速率估算）。
  const addZcodeFiles = (files: File[]) => {
    for (const file of files) {
      const id = newAttachmentId();
      const isImage = isImageFile(file);
      setAttachments((prev) => [
        ...prev,
        { id, name: file.name || (isImage ? file.name : "file"), isImage, status: "uploading" },
      ]);
      const ctrl = new AbortController();
      uploadCtrlsRef.current.set(id, ctrl);
      void uploadAttachment(sessionId, file, ctrl.signal, (loaded, total) => {
        setAttachments((prev) =>
          prev.map((a) => (a.id === id ? { ...a, loaded, total: total > 0 ? total : null } : a))
        );
      })
        .then((res) => {
          if (res === null) throw new ApiError(403, i18n.t("mobile.zcodeControl.deviceInvalid"));
          setAttachments((prev) =>
            prev.map((a) => (a.id === id ? { ...a, status: "ready", path: res.path } : a))
          );
        })
        .catch((e: unknown) => {
          if (ctrl.signal.aborted) {
            // 用户取消：chip 已随 removeZcodeAttachment 移除，静默收尾。
            setAttachments((prev) => prev.filter((a) => a.id !== id));
            return;
          }
          const reason =
            e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
          setAttachments((prev) =>
            prev.map((a) => (a.id === id ? { ...a, status: "failed", error: reason } : a))
          );
        })
        .finally(() => {
          uploadCtrlsRef.current.delete(id);
        });
    }
  };

  const removeZcodeAttachment = (id: string) => {
    // 上传中移除 = 取消（XHR abort），与 MessageComposer 同语义。
    uploadCtrlsRef.current.get(id)?.abort();
    uploadCtrlsRef.current.delete(id);
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  };

  /** 粘贴图片走同一上传链路；非图片放行默认文本行为。 */
  const handleZcodePaste = (e: ReactClipboardEvent<HTMLTextAreaElement>) => {
    const images = Array.from(e.clipboardData?.files ?? []).filter((f) => isImageFile(f));
    if (images.length === 0) return;
    e.preventDefault();
    addZcodeFiles(images);
  };

  /** 真正提交发送（一次一个 actionId）；`heldQueue` 只在用户确认处置后携带。 */
  const submitSend = (heldQueue?: HeldQueueChoice) => {
    if (!sendAllowed || session === null || inFlightRef.current) return;
    const current = attachmentsRef.current;
    const markup = attachmentMarkup(current);
    const text = draft.trim();
    if (text.length === 0 && markup.length === 0) {
      setHeldQueuePrompt(false);
      return;
    }
    // 记下本次提交原文与随行附件：accepted 清「同一份」，失败/未知全保留。
    submittedDraftRef.current = draft;
    submittedAttachmentIdsRef.current = current
      .filter((a) => a.status === "ready")
      .map((a) => a.id);
    setHeldQueuePrompt(false);
    setHeldQueueChanged(false);
    const action: ControlAction = {
      kind: "send",
      sessionId,
      generation: session.generation,
      text: markup ? (text ? `${draft}\n${markup}` : markup) : draft, // 多行原样上行
      ...(heldQueue ? { heldQueue } : {}),
    };
    void runAction(action).catch((error: unknown) => {
      if (
        heldQueue === undefined ||
        !(error instanceof ZcodeControlApiError) ||
        error.code !== "invalid_action"
      ) {
        return;
      }
      // `invalid_action` 可能是 heldQueue CAS 失配。重拉后仅当同代、已知队列的 ID
      // 集合/顺序真的变化时才重新询问；不重试发送，也不复用旧期望值。
      void (async () => {
        try {
          const fresh = await refresh();
          if (
            !aliveRef.current ||
            fresh === null ||
            fresh.sessionId !== sessionId ||
            fresh.generation !== action.generation ||
            !fresh.ready ||
            fresh.queue?.known !== true
          ) {
            return;
          }
          const currentIds = fresh.queue.items.map((item) => item.inputId);
          const expectedIds = heldQueue.expectedQueueItemIds;
          const unchanged =
            currentIds.length === expectedIds.length &&
            currentIds.every((id, index) => id === expectedIds[index]);
          if (unchanged) return;
          setHeldQueueChanged(true);
          if (fresh.inputRouting?.mode === "choice" && currentIds.length > 0) {
            setHeldQueuePrompt(true);
          }
        } catch {
          // GET 失败时保留 hook 的真实错误原因；绝不按旧队列自动重发。
        }
      })();
    });
  };

  /**
   * 忙态入队（Q6）：只创建 Tuvis 待发项，绝不直接 POST `/zcode/actions send`。
   * 成功回执不等于宿主受理；清的是**本次提交**的草稿/附件，续写的新内容保留。
   */
  const submitEnqueue = () => {
    if (!sendAllowed || session === null || inFlightRef.current) return;
    const current = attachmentsRef.current;
    const ready = current.filter((a) => a.status === "ready" && a.path !== undefined);
    if (draft.trim().length === 0 && ready.length === 0) return;
    const submittedText = draft;
    const submittedIds = ready.map((a) => a.id);
    const submittedPaths = ready.map((a) => a.path as string);
    const previous = lastEnqueueRef.current;
    const requestId =
      previous !== null &&
      previous.text === submittedText &&
      previous.paths.length === submittedPaths.length &&
      previous.paths.every((path, index) => path === submittedPaths[index])
        ? previous.requestId
        : newActionId();
    lastEnqueueRef.current = { requestId, text: submittedText, paths: submittedPaths };
    const editedFrom = editedFromRef.current;
    inFlightRef.current = true;
    setLocalQueueBusy(true);
    setLocalQueueNotice(null);
    setLocalQueueError(null);
    void enqueueZcodeMessage({
      requestId,
      sessionId,
      draft: {
        text: draft,
        attachments: ready.map((a) => ({
          name: a.name,
          path: a.path as string,
          isImage: a.isImage,
        })),
      },
      ...(editedFrom === null ? {} : { editedFrom }),
    })
      .then(async () => {
        if (!aliveRef.current) return;
        await refreshLocalQueue();
        if (!aliveRef.current) return;
        if (draftRef.current === submittedText) {
          updateDraft("");
          editedFromRef.current = null;
        }
        setAttachments((prev) => prev.filter((a) => !submittedIds.includes(a.id)));
        lastEnqueueRef.current = null;
        setLocalQueueNotice("added");
      })
      .catch((error: unknown) => {
        // 4xx 是确定拒绝（换 ID 才可能成功）；网络/5xx 保留 ID，用户重试时幂等复用。
        if (error instanceof ZcodeControlApiError && error.status !== null && error.status < 500) {
          lastEnqueueRef.current = null;
        }
        if (aliveRef.current) setLocalQueueError(queueErrorCode(error));
      })
      .finally(() => {
        inFlightRef.current = false;
        if (aliveRef.current) setLocalQueueBusy(false);
      });
  };

  const handleSend = () => {
    if (primaryAction !== "send") return;
    if (!sendAllowed || session === null || inFlightRef.current) return;
    const current = attachmentsRef.current;
    const markup = attachmentMarkup(current);
    const text = draft.trim();
    // 空正文绝不 POST（按钮是控制通道占位，不是「发空消息」的许可）。
    if (text.length === 0 && markup.length === 0) return;
    // 运行中/已有待发：先入队，绝不越过本地队列直接发上游。
    if (sendUsesOutbox) {
      submitEnqueue();
      return;
    }
    // 官方 `choice` + 非空队列：先让用户显式决定旧队列的处置（保留/清空/取消）。
    // 取消不发；确认后**一次** send（新 actionId）。UI 绝不默认清空。
    if (session.inputRouting?.mode === "choice" && (session.queue?.items.length ?? 0) > 0) {
      setHeldQueueChanged(false);
      setHeldQueuePrompt(true);
      return;
    }
    submitSend();
  };

  /** 用户确认旧队列处置：CAS 期望值取当前手机可见队列的完整 ID（服务端再核对）。 */
  const confirmHeldQueue = (disposition: HeldQueueDisposition) => {
    if (session === null) {
      setHeldQueuePrompt(false);
      setHeldQueueChanged(false);
      return;
    }
    const expectedQueueItemIds = (session.queue?.items ?? []).map((item) => item.inputId);
    submitSend({
      disposition,
      expectedQueueItemIds,
    });
  };

  const handleInterrupt = () => {
    if (primaryAction !== "stop") return;
    if (session === null || stopTarget === null || inFlightRef.current) return;
    runOnce({
      kind: "stop",
      sessionId,
      generation: session.generation,
      expectedForegroundExecutionId: stopTarget,
    });
  };

  /**
   * 主按钮唯一入口：动作由纯函数推导的结果决定（send / stop / disabled）。
   * 按钮上只有一个动作入口——绝不「先打断再发送」，也绝不在同一按钮上并列两个
   * 动作（删除独立打断行后的唯一写入口）。
   */
  const handlePrimaryAction = () => {
    if (primaryAction === "send") {
      handleSend();
      return;
    }
    if (primaryAction === "stop") {
      handleInterrupt();
    }
  };

  // ==== W4：官方队列动作（撤回 / 修改安全降级 / 重排 / 恢复运行）====
  /** 队列写面：与主按钮同一硬门（配对 + 就绪 + 代际新鲜）。 */
  const queueWritable = gateReason === null && !staleGeneration;
  /** 双层门：Tuvis `queueEdit` 能力 + 官方 `availability.queueEdit.allowed`，任一未知/关闭即关。 */
  const queueEditAllowed =
    session?.capabilities?.queueEdit?.enabled === true &&
    session.queue?.availability?.queueEditAllowed === true;
  /** 恢复自动处理的独立能力槽位（与「撤回」分开授权；未知 = 关）。 */
  const queueResumeAllowed = session?.capabilities?.queueResume?.enabled === true;
  /** 队列动作单写槽（与主按钮的 `busy` 同源，这里只用于队列入口的早退判断）。 */
  const queueBusy = queueBusyItemId !== null || queueBusyResume;

  /**
   * 轮询**权威 GET** 直到判定成立（超时 → null）。
   *
   * 只认「同会话 + 同代际 + ready + 队列已知」的**落地**投影：本地 state 可能是
   * 动作之前的快照，绝不能拿它当「已经撤回/已经恢复」的证据。`refresh()` 的返回值
   * 由 `useZcodeControl` 保证是本次真正落地的那一份。
   */
  const waitFreshQueue = async (
    generation: number,
    judge: (queue: QueueView) => boolean
  ): Promise<QueueView | null> => {
    const deadline = Date.now() + QUEUE_CONFIRM_TIMEOUT_MS;
    for (;;) {
      const view = await refresh();
      if (
        view !== null &&
        view.sessionId === sessionId &&
        view.generation === generation &&
        view.ready &&
        view.queue?.known === true &&
        judge(view.queue)
      ) {
        return view.queue;
      }
      if (Date.now() >= deadline) return null;
      await sleep(QUEUE_CONFIRM_POLL_MS);
    }
  };

  /**
   * 等一份新鲜队列说明项目是否仍在。
   *
   * 项目消失只说明「当前不在队列」，不能区分是撤回还是并发消费；`present` 是仍在，
   * `unknown` 是未拿到新鲜队列。此观察**不证明删除因果**，修改路径只提供人工复制。
   */
  const waitForQueueItemAbsent = async (
    generation: number,
    itemId: string
  ): Promise<"absent" | "present" | "unknown"> => {
    const deadline = Date.now() + QUEUE_CONFIRM_TIMEOUT_MS;
    let seen = false;
    for (;;) {
      const view = await refresh();
      if (
        view !== null &&
        view.sessionId === sessionId &&
        view.generation === generation &&
        view.ready &&
        view.queue?.known === true
      ) {
        seen = true;
        if (!view.queue.items.some((item) => item.inputId === itemId)) return "absent";
      }
      if (Date.now() >= deadline) return seen ? "present" : "unknown";
      await sleep(QUEUE_CONFIRM_POLL_MS);
    }
  };

  /**
   * 「修改」的安全降级（计划 §5.3）：请求撤回后观察新鲜队列。宿主不提供删除因果，
   * 因而项目消失时也不自动回填（它可能已被消费）；完整原文只交给用户手动复制。
   */
  const deleteAndOfferOriginal = async (args: {
    itemId: string;
    original: string;
    generation: number;
  }) => {
    setQueueBusyItemId(args.itemId);
    try {
      const receipt = await runAction({
        kind: "queueDelete",
        sessionId,
        generation: args.generation,
        queueItemId: args.itemId,
      });
      if (receipt.state === "unknown") {
        // 结果不确定：不回填、不重发；原文留在「复制原文」兜底里。
        setQueueNotice({ state: "uncertain", original: args.original });
        void refresh();
        return;
      }
      if (receipt.state === "rejected" || receipt.state === "send_failed") {
        setQueueNotice({ state: "error", reason: receipt.reasonCode ?? "unknown" });
        void refresh();
        return;
      }
      if (receipt.state === "noop") {
        setQueueNotice({ state: "unchanged" });
        void refresh();
        return;
      }
      const outcome = await waitForQueueItemAbsent(args.generation, args.itemId);
      if (outcome !== "absent") {
        setQueueNotice(
          outcome === "present"
            ? { state: "stale" }
            : { state: "uncertain", original: args.original }
        );
        void refresh();
        return;
      }
      // 防止切会话/换代后，把旧项目结果显示在新会话。
      const latest = sessionRef.current;
      if (
        latest === null ||
        latest.sessionId !== sessionId ||
        latest.generation !== args.generation
      ) {
        setQueueNotice({ state: "uncertain", original: args.original });
        return;
      }
      setQueueNotice({ state: "not-queued", original: args.original });
    } catch (error) {
      setQueueNotice({ state: "error", reason: queueErrorCode(error) });
      void refresh();
    } finally {
      setQueueBusyItemId(null);
    }
  };

  /** 「撤回」：只删除，不回填、不触碰当前草稿。 */
  const handleQueueDelete = (itemId: string) => {
    if (queueBusy || inFlightRef.current) return;
    const view = sessionRef.current;
    const item = view?.queue?.items.find((candidate) => candidate.inputId === itemId);
    if (view === null || item === undefined || !queueWritable || !queueEditAllowed) return;
    if (item.kind !== "sendText" || !dispatchWritable(item)) return;
    setQueueNotice(null);
    setCopyFallback(null);
    setCopyDone(false);
    setQueueBusyItemId(itemId);
    void (async () => {
      try {
        const receipt = await runAction({
          kind: "queueDelete",
          sessionId,
          generation: view.generation,
          queueItemId: itemId,
        });
        if (receipt.state === "unknown") {
          setQueueNotice({ state: "uncertain" });
          void refresh();
          return;
        }
        if (receipt.state === "rejected" || receipt.state === "send_failed") {
          setQueueNotice({ state: "error", reason: receipt.reasonCode ?? "unknown" });
          void refresh();
          return;
        }
        if (receipt.state === "noop") {
          setQueueNotice({ state: "unchanged" });
          void refresh();
          return;
        }
        // ACK 只到「撤回中」：完成只能由新鲜队列确认（项消失）。
        const outcome = await waitForQueueItemAbsent(view.generation, itemId);
        if (outcome === "absent") setQueueNotice({ state: "not-queued" });
        else setQueueNotice(outcome === "present" ? { state: "stale" } : { state: "uncertain" });
        void refresh();
      } catch (error) {
        setQueueNotice({ state: "error", reason: queueErrorCode(error) });
        void refresh();
      } finally {
        setQueueBusyItemId(null);
      }
    })();
  };

  /** 「修改」：先取完整原文；队列消失后只提供人工复制，不自动回填或发送。 */
  const handleQueueEdit = (itemId: string) => {
    if (queueBusy || inFlightRef.current) return;
    const view = sessionRef.current;
    const item = view?.queue?.items.find((candidate) => candidate.inputId === itemId);
    if (view === null || item === undefined || !queueWritable || !queueEditAllowed) return;
    if (item.kind !== "sendText" || item.hasAttachments || !dispatchWritable(item)) return;
    const original = item.text;
    setQueueNotice(null);
    setCopyFallback(null);
    setCopyDone(false);
    void deleteAndOfferOriginal({
      itemId,
      original,
      generation: view.generation,
    });
  };

  /** 「移到最前/上移/下移」：目标一律是**显式**官方项 ID（绝不传 null / 不猜下标）。 */
  const handleQueueMove = (itemId: string, beforeQueueItemId: string) => {
    if (queueBusy || inFlightRef.current) return;
    const view = sessionRef.current;
    if (view === null || !queueWritable || !queueEditAllowed) return;
    // 队列已变（目标项消失）→ 不提交：服务端也会拒，这里先收手等下一次刷新。
    if (!view.queue?.items.some((item) => item.inputId === beforeQueueItemId)) return;
    setQueueNotice(null);
    setQueueBusyItemId(itemId);
    void (async () => {
      try {
        const receipt = await runAction({
          kind: "queueReorder",
          sessionId,
          generation: view.generation,
          queueItemId: itemId,
          beforeQueueItemId,
        });
        if (receipt.state === "accepted" || receipt.state === "confirmed") {
          void refresh();
          return;
        }
        setQueueNotice(
          receipt.state === "unknown"
            ? { state: "uncertain" }
            : { state: "error", reason: receipt.reasonCode ?? "unknown" }
        );
        void refresh();
      } catch (error) {
        setQueueNotice({ state: "error", reason: queueErrorCode(error) });
        void refresh();
      } finally {
        setQueueBusyItemId(null);
      }
    })();
  };

  /**
   * 「恢复运行」：只发 `queueResume`（绝不自动发送/自动点第二条）。只有新鲜队列
   * `paused=false` 才显示「已恢复」——那不代表被打断的回合会重启，也不代表下一条
   * 已经在跑（文案不暗示）。
   */
  const handleQueueResume = () => {
    if (queueBusy || inFlightRef.current) return;
    const view = sessionRef.current;
    if (view === null || !queueWritable || !queueResumeAllowed) return;
    if (view.queue?.paused !== true) return;
    setQueueNotice(null);
    setResumeConfirmed(false);
    setLocalQueueBusy(false);
    setLocalQueueNotice(null);
    setLocalQueueError(null);
    setPriorityPrompt(null);
    setEditConflict(null);
    setEditPreflight(null);
    setQueueRecovery(null);
    editedFromRef.current = null;
    setQueueBusyResume(true);
    void (async () => {
      try {
        const receipt = await runAction({
          kind: "queueResume",
          sessionId,
          generation: view.generation,
        });
        if (receipt.state === "unknown") {
          setQueueNotice({ state: "uncertain" });
          void refresh();
          return;
        }
        if (receipt.state === "rejected" || receipt.state === "send_failed") {
          setQueueNotice({ state: "error", reason: receipt.reasonCode ?? "unknown" });
          void refresh();
          return;
        }
        if (receipt.state === "noop") {
          setQueueNotice({ state: "unchanged" });
          void refresh();
          return;
        }
        const queue = await waitFreshQueue(view.generation, (fresh) => fresh.paused === false);
        if (queue === null) {
          setQueueNotice({ state: "uncertain" });
          void refresh();
          return;
        }
        setResumeConfirmed(true);
      } catch (error) {
        setQueueNotice({ state: "error", reason: queueErrorCode(error) });
        void refresh();
      } finally {
        setQueueBusyResume(false);
      }
    })();
  };

  // ==== Q6：Tuvis 本地待发（入队/插队/再编辑/撤回/继续）====
  const localQueueWritable = gateReason === null && !staleGeneration;

  // Unknown/停止/派发/重启屏障的人工核对入口：只在有本地队列时读取；失败不污染写面。
  const localQueueVersion = localQueue?.version ?? null;
  useEffect(() => {
    if (localQueueVersion === null) {
      setQueueRecovery(null);
      return;
    }
    let alive = true;
    void getZcodeQueueRecovery(sessionId)
      .then((view) => {
        if (alive) setQueueRecovery(view);
      })
      .catch(() => {
        if (alive) setQueueRecovery(null);
      });
    return () => {
      alive = false;
    };
  }, [sessionId, localQueueVersion]);

  const refreshLocalView = async () => {
    await refreshLocalQueue();
    // 会话投影也重拉：官方运行态/队列可能在同一帧变化，单条编排的后续判断要新鲜。
    void refresh();
  };

  const runPriority = (itemId: string, version: number, executionId: string | null) => {
    if (!localQueueWritable || inFlightRef.current) return;
    const view = sessionRef.current;
    if (view === null) return;
    const heldQueue =
      view.inputRouting?.mode === "choice" && (view.queue?.items.length ?? 0) > 0
        ? {
            disposition: "keepQueueAndSend" as const,
            expectedQueueItemIds: view.queue?.items.map((item) => item.inputId) ?? [],
          }
        : undefined;
    setPriorityPrompt(null);
    setLocalQueueNotice(null);
    setLocalQueueError(null);
    inFlightRef.current = true;
    setLocalQueueBusy(true);
    void prioritizeZcodeMessage({
      requestId: newActionId(),
      itemId,
      expectedVersion: version,
      confirmStop: executionId !== null,
      expectedExecutionId: executionId,
      ...(heldQueue === undefined ? {} : { heldQueue }),
    })
      .then(async (operation) => {
        if (!aliveRef.current) return;
        if (operation.state === "failed") {
          const code = operation.receipt?.reasonCode ?? operation.stopFailure?.code ?? "unknown";
          setLocalQueueError(isZcodeErrorCode(code) ? code : "unknown");
        } else if (operation.state === "completed") {
          setLocalQueueNotice("priority_done");
        } else {
          // in_flight/未知都不能显示成“已发送”；只提示待核对。
          setLocalQueueNotice("priority_unknown");
        }
        await refreshLocalView();
      })
      .catch((error: unknown) => {
        if (aliveRef.current) setLocalQueueError(queueErrorCode(error));
      })
      .finally(() => {
        inFlightRef.current = false;
        if (aliveRef.current) setLocalQueueBusy(false);
      });
  };

  const handleLocalPrioritize = (itemId: string, version: number) => {
    if (!localQueueWritable || inFlightRef.current) return;
    if (!running) {
      runPriority(itemId, version, null);
      return;
    }
    if (stopTarget === null) {
      setLocalQueueError("target_ambiguous");
      return;
    }
    setLocalQueueError(null);
    setLocalQueueNotice(null);
    setPriorityPrompt({ itemId, version });
  };

  const applyEditResult = (result: EditResult, merge: boolean) => {
    const restored = result.draft.attachments.map((attachment) => ({
      id: newAttachmentId(),
      name: attachment.name,
      isImage: attachment.isImage,
      status: "ready" as const,
      path: attachment.path,
    }));
    if (merge) {
      updateDraft(
        draftRef.current.length === 0
          ? result.draft.text
          : `${draftRef.current}\n${result.draft.text}`
      );
      setAttachments((prev) => [...prev, ...restored]);
    } else {
      updateDraft(result.draft.text);
      setAttachments(restored);
    }
    editedFromRef.current = result.requestId;
    setEditConflict(null);
    setLocalQueueNotice("edit_restored");
    void refreshLocalView();
  };

  /** 附件版本戳：正文/附件在请求期间任一变化，替换都必须重新确认。 */
  const attachmentStamp = () =>
    attachmentsRef.current
      .map(
        (attachment) =>
          `${attachment.id}:${attachment.status}:${attachment.path}:${attachment.name}`
      )
      .join("\u0001");

  const runLocalEdit = (
    itemId: string,
    version: number,
    intent: "auto" | "replace" | "merge",
    expectedReplace?: { text: string; attachments: string }
  ) => {
    if (!localQueueWritable || inFlightRef.current) return;
    setLocalQueueNotice(null);
    setLocalQueueError(null);
    inFlightRef.current = true;
    setLocalQueueBusy(true);
    void editZcodeQueuedMessage({ requestId: newActionId(), itemId, expectedVersion: version })
      .then((result) => {
        if (!aliveRef.current) return;
        if (intent === "merge") {
          applyEditResult(result, true);
          return;
        }
        if (
          (intent === "auto" &&
            (draftRef.current.trim().length > 0 ||
              attachmentsRef.current.some((a) => a.status === "ready"))) ||
          (intent === "replace" &&
            expectedReplace !== undefined &&
            (draftRef.current !== expectedReplace.text ||
              attachmentStamp() !== expectedReplace.attachments))
        ) {
          // 请求期间草稿/附件变化：不覆盖，交给显式选择；取消不删快照。
          setEditConflict({ requestId: result.requestId, result });
          return;
        }
        applyEditResult(result, false);
      })
      .catch((error: unknown) => {
        if (aliveRef.current) setLocalQueueError(queueErrorCode(error));
      })
      .finally(() => {
        inFlightRef.current = false;
        if (aliveRef.current) setLocalQueueBusy(false);
      });
  };

  const handleLocalEdit = (itemId: string, version: number) => {
    if (!localQueueWritable || inFlightRef.current) return;
    // 先处理已知草稿冲突：取消时零 edit POST，原排队项不被撤回、也不产生快照。
    if (
      draftRef.current.trim().length > 0 ||
      attachmentsRef.current.some((a) => a.status === "ready")
    ) {
      setLocalQueueError(null);
      setEditPreflight({ itemId, version });
      return;
    }
    runLocalEdit(itemId, version, "auto");
  };

  const handleRecoverEdit = (requestId: string) => {
    if (!localQueueWritable || inFlightRef.current) return;
    setLocalQueueError(null);
    inFlightRef.current = true;
    setLocalQueueBusy(true);
    void getZcodeEditSnapshot(requestId)
      .then((result) => {
        if (!aliveRef.current) return;
        if (
          draftRef.current.trim().length > 0 ||
          attachmentsRef.current.some((a) => a.status === "ready")
        ) {
          setEditConflict({ requestId, result });
        } else {
          applyEditResult(result, false);
        }
      })
      .catch((error: unknown) => {
        if (aliveRef.current) setLocalQueueError(queueErrorCode(error));
      })
      .finally(() => {
        inFlightRef.current = false;
        if (aliveRef.current) setLocalQueueBusy(false);
      });
  };

  const handleEditConflictCancel = () => {
    // 已经撤回的项其快照是唯一恢复入口：取消只关对话框，绝不 finish/删除。
    setEditConflict(null);
    void refreshLocalView();
  };

  const handleLocalRetract = (itemId: string, version: number) => {
    if (!localQueueWritable || inFlightRef.current) return;
    setLocalQueueNotice(null);
    setLocalQueueError(null);
    inFlightRef.current = true;
    setLocalQueueBusy(true);
    void retractZcodeQueuedMessage({ requestId: newActionId(), itemId, expectedVersion: version })
      .then(() => {
        if (!aliveRef.current) return;
        setLocalQueueNotice("retracted");
        return refreshLocalView();
      })
      .catch((error: unknown) => {
        if (aliveRef.current) setLocalQueueError(queueErrorCode(error));
      })
      .finally(() => {
        inFlightRef.current = false;
        if (aliveRef.current) setLocalQueueBusy(false);
      });
  };

  const handleLocalResume = () => {
    if (!localQueueWritable || inFlightRef.current || localQueue === null) return;
    setLocalQueueNotice(null);
    setLocalQueueError(null);
    inFlightRef.current = true;
    setLocalQueueBusy(true);
    void resumeZcodeLocalQueue({
      requestId: newActionId(),
      sessionId,
      expectedQueueVersion: localQueue.version,
    })
      .then(() => {
        if (!aliveRef.current) return;
        setLocalQueueNotice("resumed");
        return refreshLocalView();
      })
      .catch((error: unknown) => {
        if (aliveRef.current) setLocalQueueError(queueErrorCode(error));
      })
      .finally(() => {
        inFlightRef.current = false;
        if (aliveRef.current) setLocalQueueBusy(false);
      });
  };

  const acknowledgeRecovery = (
    kind: "priority_stop" | "manual_stop" | "dispatch" | "unconfirmed",
    subjectId: string
  ) => {
    if (!localQueueWritable || inFlightRef.current) return;
    setLocalQueueNotice(null);
    setLocalQueueError(null);
    inFlightRef.current = true;
    setLocalQueueBusy(true);
    void acknowledgeZcodeQueueRecovery({
      requestId: newActionId(),
      sessionId,
      kind,
      subjectId,
    })
      .then(async () => {
        if (!aliveRef.current) return;
        setLocalQueueNotice("recovery_reviewed");
        await refreshLocalView();
        const view = await getZcodeQueueRecovery(sessionId).catch(() => null);
        if (aliveRef.current) setQueueRecovery(view);
      })
      .catch((error: unknown) => {
        if (aliveRef.current) setLocalQueueError(queueErrorCode(error));
      })
      .finally(() => {
        inFlightRef.current = false;
        if (aliveRef.current) setLocalQueueBusy(false);
      });
  };

  /** 「复制原文」人工兜底：剪贴板不可用（非安全上下文/jsdom）→ 只读文本框给全文。 */
  const copyOriginal = async (text: string) => {
    setCopyDone(false);
    try {
      if (navigator.clipboard?.writeText !== undefined) {
        await navigator.clipboard.writeText(text);
        setCopyDone(true);
        return;
      }
    } catch {
      // 权限/非安全上下文：落到只读文本框。
    }
    setCopyFallback(text);
  };

  // 模型/档位：目录只来自 session.selection；供应商分组来自官方目录。provider
  // 只改变本地待选组合（零 POST），选定模型后才提交一次完整三元组。
  const selection = session?.selection ?? null;
  const [pendingProvider, setPendingProvider] = useState<string | null>(null);
  const [pendingModel, setPendingModel] = useState<string | null>(null);
  const providerGroups = selection?.providers ?? [];
  const activeProviderId = pendingProvider ?? selection?.provider ?? "";
  const activeProvider =
    providerGroups.find(
      (candidate) => candidate.providerId === activeProviderId && candidate.available
    ) ?? null;
  const activeModels =
    activeProvider?.models ??
    (selection !== null && activeProviderId === selection.provider ? selection.models : []);
  const preferredModel =
    pendingModel ??
    (selection !== null && activeProviderId === selection.provider ? selection.model : null);
  const currentChoice =
    activeModels.find((choice) => choice.modelId === preferredModel) ?? activeModels[0] ?? null;
  // 官方配置落地（或切会话清空）后回到服务端当前组合；失败/在途期间保留用户待选值。
  useEffect(() => {
    setPendingProvider(null);
    setPendingModel(null);
  }, [selection?.provider, selection?.model, selection?.thought]);

  // ==== R10：协作模式（独立能力槽；ACK 不乐观改确认标签）====
  const modeView = selection?.mode ?? null;
  const effectiveMode = modeView?.effectiveMode ?? null;
  const [pendingMode, setPendingMode] = useState<CollaborationMode | "">("");
  const [modeConfirmOpen, setModeConfirmOpen] = useState(false);
  useEffect(() => {
    setPendingMode(effectiveMode ?? "");
    setModeConfirmOpen(false);
  }, [effectiveMode, sessionId]);
  const modeLabels: Record<CollaborationMode, string> = {
    plan: "mobile.zcodeControl.modePlan",
    build: "mobile.zcodeControl.modeBuild",
    guarded: "mobile.zcodeControl.modeGuarded",
    yolo: "mobile.zcodeControl.modeYolo",
    edit: "mobile.zcodeControl.modeEdit",
  };
  const modeEnabled =
    gateReason === null &&
    session !== null &&
    session.capabilities.mode?.enabled === true &&
    !staleGeneration &&
    modeView !== null;
  const submitMode = (mode: CollaborationMode) => {
    if (session === null || !modeEnabled) return;
    runOnce({
      kind: "mode",
      sessionId,
      generation: session.generation,
      mode,
      confirmFullAccess: mode === "yolo",
    });
  };
  const applyMode = () => {
    if (pendingMode === "" || pendingMode === effectiveMode) return;
    if (pendingMode === "yolo") {
      setModeConfirmOpen(true);
      return;
    }
    submitMode(pendingMode);
  };
  const selectionEnabled =
    gateReason === null &&
    session?.capabilities.model.enabled === true &&
    session.capabilities.thoughtLevel.enabled === true;
  const selectionReason =
    gateReason ??
    session?.capabilities.model.reasonCode ??
    session?.capabilities.thoughtLevel.reasonCode ??
    "unknown";
  const currentThought =
    selection?.thought != null && currentChoice?.thoughtLevels.includes(selection.thought)
      ? selection.thought
      : (currentChoice?.thoughtLevels[0] ?? "");

  const submitSelection = (model: string, thought: string, provider?: string) => {
    if (session === null || !selectionEnabled || thought === "") return;
    runOnce({
      kind: "selection",
      sessionId,
      generation: session.generation,
      ...(provider === undefined ? {} : { provider }),
      model,
      thought,
    });
  };

  const handleProviderChange = (providerId: string) => {
    if (selection === null || providerId === activeProviderId) return;
    if (!providerGroups.some((group) => group.providerId === providerId && group.available)) {
      return;
    }
    // 只更新待选组合；不写 provider-only 中间态。
    setPendingProvider(providerId);
    setPendingModel(null);
  };

  const handleModelChange = (modelId: string) => {
    if (selection === null) return;
    const resolved = resolveModelChoice(selection, activeProviderId, modelId);
    if (resolved === null) return;
    setPendingModel(resolved.model);
    submitSelection(
      resolved.model,
      resolved.thought,
      providerGroups.length === 0 ? undefined : activeProviderId
    );
  };

  const handleThoughtChange = (thought: string) => {
    if (selection === null || currentChoice?.thoughtLevels.includes(thought) !== true) return;
    submitSelection(
      currentChoice.modelId,
      thought,
      providerGroups.length === 0 ? undefined : activeProviderId
    );
  };

  return (
    <div
      data-testid="zcode-control-panel"
      data-connection-state={connection?.state ?? "unknown"}
      className="shrink-0 border-t border-[var(--cb)] px-3 py-2"
    >
      {displayReason !== null && (
        <p
          data-testid="zcode-control-reason"
          data-reason-code={displayReason}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {zcodeReasonText(displayReason)}
        </p>
      )}
      {/* T8 冲突态「断开并抢回」：只在 conflict 出现。确认文案常驻按钮旁（点击即
          执行一次），busy 禁用；仍 conflict 只提示先关官方页面，失败显示冻结码原因。 */}
      {connection?.state === "conflict" && (
        <div className="mb-2">
          <button
            type="button"
            data-testid="zcode-takeover"
            disabled={takeoverBusy}
            onClick={handleTakeover}
            className="rounded-full border border-[var(--cb)] px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t(
              takeoverBusy ? "mobile.zcodeControl.takeoverBusy" : "mobile.zcodeControl.takeover"
            )}
          </button>
          <p data-testid="zcode-takeover-hint" className="mt-1 text-xs text-[var(--mut)]">
            {i18n.t("mobile.zcodeControl.takeoverHint")}
          </p>
          {takeoverStillConflict && (
            <p
              data-testid="zcode-takeover-still-conflict"
              className="mt-1 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.zcodeControl.takeoverStillConflict")}
            </p>
          )}
          {takeoverError !== null && (
            <p
              data-testid="zcode-takeover-error"
              data-reason-code={takeoverError}
              className="mt-1 text-xs text-[var(--mut)]"
            >
              {zcodeReasonText(takeoverError)}
            </p>
          )}
        </div>
      )}
      {receipt !== null && (
        <p
          data-testid="zcode-action-status"
          data-action-state={receipt.state}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {i18n.t(STATUS_KEYS[receipt.state] ?? "mobile.zcodeControl.statusUnknown")}
        </p>
      )}
      {/* 多张 pending 并存：默认只展示一张「当前交互卡」，其余用紧凑小标签切换
          （「问答 1/3」「审批 2/3」）。切组不删卡——每张卡各自保留草稿与受理锁，
          绝不会把其他待处理请求静默丢掉。 */}
      {pendings.length > 1 && (
        <div
          data-testid="zcode-pending-tabs"
          role="tablist"
          aria-label={i18n.t("mobile.zcodeControl.pendingTabsAria")}
          className="mb-1 flex flex-wrap gap-1"
        >
          {pendings.map((pending, index) => {
            const selected = pending.interactionId === activePendingId;
            return (
              <button
                key={pending.interactionId}
                id={pendingDomId("zcode-pending-tab", pending.interactionId)}
                type="button"
                role="tab"
                aria-selected={selected}
                aria-controls={pendingDomId("zcode-pending-panel", pending.interactionId)}
                data-testid={`zcode-pending-tab-${pending.interactionId}`}
                data-active={selected ? "true" : "false"}
                onClick={() => setPendingTabId(pending.interactionId)}
                className={`rounded-full border px-2 py-0.5 text-xs ${
                  selected
                    ? "border-[var(--btnp)] bg-[var(--btnp)]/10 text-[var(--tx)]"
                    : "border-[var(--cb)] text-[var(--mut)]"
                }`}
              >
                {i18n.t(
                  pending.kind === "permission"
                    ? "mobile.zcodeControl.pendingTabPermission"
                    : "mobile.zcodeControl.pendingTabQuestion",
                  { position: index + 1, total: pendings.length }
                )}
              </button>
            );
          })}
        </div>
      )}
      {session !== null &&
        pendings.map((pending) => (
          // key 带代际：interaction 被替换或换代时整卡重挂（草稿/翻页/受理锁一并清空）。
          // 非当前组仍挂载但视觉隐藏：草稿与锁定各自保留，切回来即恢复。
          <div
            key={`${session.generation}:${pending.interactionId}`}
            id={pendingDomId("zcode-pending-panel", pending.interactionId)}
            // 多组并存才成组：只有那时这个容器才是某个 tab 的 tabpanel，
            // 单张卡没有 tab 指向它，不冒充 tabpanel 语义（避免悬空 aria-*）。
            role={pendings.length > 1 ? "tabpanel" : undefined}
            aria-labelledby={
              pendings.length > 1
                ? pendingDomId("zcode-pending-tab", pending.interactionId)
                : undefined
            }
            data-testid={`zcode-pending-slot-${pending.interactionId}`}
            data-active={pending.interactionId === activePendingId ? "true" : "false"}
            // hidden 与 inert 同时用：hidden 管视觉隐藏，inert 在**真实浏览器**里
            // 阻断隐藏组的 Tab/读屏/程序化激活（jsdom 未实现 inert 语义——其
            // focusing.js 里 inert 只有 TODO，故自动测试只能断属性存在；自动回环
            // 的实际隔离护栏是 hidden + 组内控件显式断言，浏览器侧语义列入实机待验）。
            // 多组卡内层 testid 同名，测试也必须 within(slot)。
            hidden={pending.interactionId !== activePendingId}
            inert={pending.interactionId !== activePendingId}
          >
            <ZcodePendingCard
              pending={pending}
              sessionId={sessionId}
              generation={session.generation}
              capabilities={session.capabilities}
              gateReason={gateReason}
              submitting={submitting}
              revision={session.revision}
              submit={runAction}
              // 只读卡/截断审批需要「打开官方页面」；链接未知时不挂入口（绝不伪造 URL）。
              onOpenOfficial={officialUrl === null ? undefined : openOfficial}
            />
          </div>
        ))}
      {queueRecovery !== null &&
        (queueRecovery.priority.length > 0 ||
          queueRecovery.manualStopRequestId !== null ||
          queueRecovery.dispatchActionId !== null ||
          queueRecovery.unconfirmedActionId !== null) && (
          <section
            data-testid="zcode-local-recovery"
            className="mb-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2"
          >
            <p className="text-xs font-medium text-[var(--tx)]">
              {i18n.t("mobile.zcodeControl.localQueueRecoveryTitle")}
            </p>
            <p className="mt-1 text-[11px] leading-4 text-[var(--mut)]">
              {i18n.t("mobile.zcodeControl.localQueueRecoveryHint")}
            </p>
            <div className="mt-2 flex flex-wrap gap-2">
              {queueRecovery.priority.map((entry) => (
                <button
                  key={`priority-${entry.requestId}`}
                  type="button"
                  data-testid="zcode-recovery-priority"
                  data-subject-id={entry.requestId}
                  disabled={busy || !localQueueWritable}
                  onClick={() => acknowledgeRecovery("priority_stop", entry.requestId)}
                  className="rounded-full border border-[var(--cb)] px-2 py-1 text-[11px] text-[var(--tx)] disabled:opacity-40"
                >
                  {i18n.t("mobile.zcodeControl.localQueueRecoveryPriority")}
                </button>
              ))}
              {queueRecovery.manualStopRequestId !== null && (
                <button
                  type="button"
                  data-testid="zcode-recovery-manual-stop"
                  disabled={busy || !localQueueWritable}
                  onClick={() =>
                    acknowledgeRecovery("manual_stop", queueRecovery.manualStopRequestId as string)
                  }
                  className="rounded-full border border-[var(--cb)] px-2 py-1 text-[11px] text-[var(--tx)] disabled:opacity-40"
                >
                  {i18n.t("mobile.zcodeControl.localQueueRecoveryStop")}
                </button>
              )}
              {queueRecovery.dispatchActionId !== null && (
                <button
                  type="button"
                  data-testid="zcode-recovery-dispatch"
                  disabled={busy || !localQueueWritable}
                  onClick={() =>
                    acknowledgeRecovery("dispatch", queueRecovery.dispatchActionId as string)
                  }
                  className="rounded-full border border-[var(--cb)] px-2 py-1 text-[11px] text-[var(--tx)] disabled:opacity-40"
                >
                  {i18n.t("mobile.zcodeControl.localQueueRecoveryDispatch")}
                </button>
              )}
              {queueRecovery.unconfirmedActionId !== null && (
                <button
                  type="button"
                  data-testid="zcode-recovery-unconfirmed"
                  disabled={busy || !localQueueWritable}
                  onClick={() =>
                    acknowledgeRecovery("unconfirmed", queueRecovery.unconfirmedActionId as string)
                  }
                  className="rounded-full border border-[var(--cb)] px-2 py-1 text-[11px] text-[var(--tx)] disabled:opacity-40"
                >
                  {i18n.t("mobile.zcodeControl.localQueueRecoveryRestart")}
                </button>
              )}
            </div>
          </section>
        )}
      {/* Tuvis 本地待发（Q6）：与官方队列分开来源；默认展开，显示位次/摘要/三按钮。 */}
      <ZcodeLocalQueuePanel
        view={localQueue}
        busy={
          busy ||
          !localQueueWritable ||
          editConflict !== null ||
          editPreflight !== null ||
          priorityPrompt !== null
        }
        onPrioritize={handleLocalPrioritize}
        onEdit={handleLocalEdit}
        onRetract={handleLocalRetract}
        onResume={handleLocalResume}
        onRecoverEdit={handleRecoverEdit}
      />
      {localQueueNotice !== null && (
        <p
          data-testid="zcode-local-queue-notice"
          data-state={localQueueNotice}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {i18n.t(
            localQueueNotice === "added"
              ? "mobile.zcodeControl.localQueueAdded"
              : localQueueNotice === "priority_unknown"
                ? "mobile.zcodeControl.localQueuePriorityUnknown"
                : localQueueNotice === "priority_done"
                  ? "mobile.zcodeControl.localQueuePriorityDone"
                  : localQueueNotice === "retracted"
                    ? "mobile.zcodeControl.localQueueRetractDone"
                    : localQueueNotice === "resumed"
                      ? "mobile.zcodeControl.localQueueResumeDone"
                      : localQueueNotice === "edit_restored"
                        ? "mobile.zcodeControl.localQueueEditRestored"
                        : localQueueNotice === "recovery_reviewed"
                          ? "mobile.zcodeControl.localQueueRecoveryReviewed"
                          : "mobile.zcodeControl.localQueueEditConflict"
          )}
        </p>
      )}
      {localQueueError !== null && (
        <p
          data-testid="zcode-local-queue-error"
          data-reason-code={localQueueError}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {zcodeReasonText(localQueueError)}
        </p>
      )}
      {priorityPrompt !== null && (
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="zcode-priority-title"
          data-testid="zcode-priority-dialog"
          className="mb-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2"
        >
          <p id="zcode-priority-title" className="text-xs font-medium text-[var(--tx)]">
            {i18n.t("mobile.zcodeControl.localQueuePriorityTitle")}
          </p>
          <p className="mt-1 text-[11px] leading-4 text-[var(--mut)]">
            {i18n.t("mobile.zcodeControl.localQueuePriorityHint")}
          </p>
          <div className="mt-2 flex flex-wrap gap-2">
            <button
              type="button"
              data-testid="zcode-priority-confirm"
              onClick={() => {
                const current = sessionRef.current;
                const execution = current?.control?.activeWorks.filter(
                  (work) => work.kind === "primaryTurn" && work.foregroundExecutionId !== null
                );
                if (current === null || execution?.length !== 1) {
                  setPriorityPrompt(null);
                  setLocalQueueError("target_ambiguous");
                  return;
                }
                runPriority(
                  priorityPrompt.itemId,
                  priorityPrompt.version,
                  execution[0].foregroundExecutionId
                );
              }}
              className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.localQueuePriorityConfirm")}
            </button>
            <button
              type="button"
              data-testid="zcode-priority-cancel"
              onClick={() => setPriorityPrompt(null)}
              className="rounded-lg px-3 py-1.5 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.zcodeControl.localQueuePriorityCancel")}
            </button>
          </div>
        </div>
      )}
      {editPreflight !== null && (
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="zcode-edit-preflight-title"
          data-testid="zcode-edit-preflight-dialog"
          className="mb-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2"
        >
          <p id="zcode-edit-preflight-title" className="text-xs font-medium text-[var(--tx)]">
            {i18n.t("mobile.zcodeControl.localQueueEditConflict")}
          </p>
          <p className="mt-1 text-[11px] leading-4 text-[var(--mut)]">
            {i18n.t("mobile.zcodeControl.localQueueEditPreflightHint")}
          </p>
          <div className="mt-2 flex flex-wrap gap-2">
            <button
              type="button"
              data-testid="zcode-edit-preflight-replace"
              onClick={() => {
                const current = editPreflight;
                setEditPreflight(null);
                if (current !== null)
                  runLocalEdit(current.itemId, current.version, "replace", {
                    text: draftRef.current,
                    attachments: attachmentStamp(),
                  });
              }}
              className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.localQueueEditReplace")}
            </button>
            <button
              type="button"
              data-testid="zcode-edit-preflight-merge"
              onClick={() => {
                const current = editPreflight;
                setEditPreflight(null);
                if (current !== null) runLocalEdit(current.itemId, current.version, "merge");
              }}
              className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.localQueueEditMerge")}
            </button>
            <button
              type="button"
              data-testid="zcode-edit-preflight-cancel"
              onClick={() => setEditPreflight(null)}
              className="rounded-lg px-3 py-1.5 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.zcodeControl.localQueueEditCancel")}
            </button>
          </div>
        </div>
      )}
      {editConflict !== null && (
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="zcode-edit-conflict-title"
          data-testid="zcode-edit-conflict-dialog"
          className="mb-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2"
        >
          <p id="zcode-edit-conflict-title" className="text-xs font-medium text-[var(--tx)]">
            {i18n.t("mobile.zcodeControl.localQueueEditConflict")}
          </p>
          <p className="mt-1 text-[11px] leading-4 whitespace-pre-wrap text-[var(--mut)]">
            {zcodeQueuePreview(editConflict.result.draft.text, 80)}
          </p>
          <div className="mt-2 flex flex-wrap gap-2">
            <button
              type="button"
              data-testid="zcode-edit-conflict-replace"
              onClick={() => applyEditResult(editConflict.result, false)}
              className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.localQueueEditReplace")}
            </button>
            <button
              type="button"
              data-testid="zcode-edit-conflict-merge"
              onClick={() => applyEditResult(editConflict.result, true)}
              className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.localQueueEditMerge")}
            </button>
            <button
              type="button"
              data-testid="zcode-edit-conflict-cancel"
              onClick={handleEditConflictCancel}
              className="rounded-lg px-3 py-1.5 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.zcodeControl.localQueueEditCancel")}
            </button>
          </div>
        </div>
      )}
      {/* 官方输入队列（W4）：默认一行（数量 + 暂停 + 恢复运行），点开才列条目。
          每行「序号 → 20 字素预览 → 引导/修改/撤回」；所有写动作只传官方 inputId，
          由本面板负责生成信封、验证新鲜队列与安全降级。未知/空队列不占位。 */}
      <ZcodeQueuePanel
        queue={session?.queue}
        canWrite={queueWritable}
        writeBusy={busy}
        queueEditAllowed={queueEditAllowed}
        queueResumeAllowed={queueResumeAllowed}
        busyItemId={queueBusyItemId}
        busyResume={queueBusyResume}
        resumeConfirmed={resumeConfirmed}
        onEditToDraft={handleQueueEdit}
        onDelete={handleQueueDelete}
        onMove={handleQueueMove}
        onResume={handleQueueResume}
      />
      {/* 队列动作结果：状态以新鲜队列为准；项目消失不证明删除因果。 */}
      {queueNotice !== null && (
        <p
          data-testid="zcode-queue-notice"
          data-state={queueNotice.state}
          data-reason-code={queueNotice.state === "error" ? queueNotice.reason : undefined}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {queueNotice.state === "error"
            ? zcodeReasonText(queueNotice.reason)
            : i18n.t(
                queueNotice.state === "not-queued"
                  ? "mobile.zcodeControl.queueEditRemovalUncertain"
                  : queueNotice.state === "stale"
                    ? "mobile.zcodeControl.queueItemStillQueued"
                    : queueNotice.state === "unchanged"
                      ? "mobile.zcodeControl.queueUnchanged"
                      : "mobile.zcodeControl.queueResultUncertain"
              )}
        </p>
      )}
      {(queueNotice?.state === "uncertain" || queueNotice?.state === "not-queued") &&
        queueNotice.original !== undefined && (
          <div className="mb-2 flex flex-wrap items-center gap-2">
            <button
              type="button"
              data-testid="zcode-queue-copy-original"
              onClick={() => void copyOriginal(queueNotice.original ?? "")}
              className="rounded-full border border-[var(--cb)] px-3 py-1 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.queueCopyOriginal")}
            </button>
            {copyDone && (
              <span data-testid="zcode-queue-copied" className="text-xs text-[var(--mut)]">
                {i18n.t("mobile.zcodeControl.queueCopied")}
              </span>
            )}
          </div>
        )}
      {/* 剪贴板不可用时的人工兜底：全文只读，供手动选择复制（不自动发送）。 */}
      {copyFallback !== null && (
        <textarea
          data-testid="zcode-queue-copy-fallback"
          aria-label={i18n.t("mobile.zcodeControl.queueCopyOriginal")}
          readOnly
          value={copyFallback}
          rows={3}
          className="mb-2 w-full resize-none rounded-lg border border-[var(--cb)] px-2 py-1 text-xs text-[var(--tx)]"
        />
      )}
      {/* ==== 单行紧凑工具栏（R4/U2）====
          「打开官方页面」+ 模型 + 思考强度 三者同一容器、同一行；行为与协议
          零变化（openOfficial 只开已保存链接，模型/档位仍发同形状 selection）。
          布局裁决（计划 §1）：
          - 360px/100% 字号：保持一行（模型 select 可伸缩，长模型名省略显示，
            但全名保留在 title 与 option 原文里，键盘/读屏仍可取全名）；
          - 320px/125% 等极窄场景：允许折到第二行（容器可换行 + 模型 select
            有最小可用宽度），绝不横向溢出、绝不隐藏必要动作；
          - 「打开官方页面」与档位 select 不收缩（shrink-0）：动作不被裁掉。
          禁用原因另起一行放在工具栏下方：长句不参与行内排版。 */}
      {(officialUrl !== null || selection !== null) && (
        <div
          data-testid="zcode-control-toolbar"
          data-layout="single-row"
          data-max-rows="2"
          className="mb-2 flex flex-wrap items-center gap-x-2 gap-y-1"
        >
          {/* A5 显式辅助入口：只打开**已保存**的官方链接（挂载不自动外跳、不代理点击）。 */}
          {officialUrl !== null && (
            <button
              type="button"
              data-testid="zcode-open-official"
              onClick={openOfficial}
              className="shrink-0 rounded-full border border-[var(--cb)] px-2 py-1 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.composer.zcodeOpenOfficial")}
            </button>
          )}
          {selection !== null && (
            <>
              {providerGroups.length > 0 && (
                <select
                  data-testid="zcode-provider-select"
                  aria-label={i18n.t("mobile.zcodeControl.providerAria")}
                  value={activeProviderId}
                  disabled={!selectionEnabled}
                  onChange={(e) => handleProviderChange(e.target.value)}
                  className="max-w-[45%] shrink-0 rounded-lg border border-[var(--cb)] px-2 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
                >
                  {providerGroups.map((group) => (
                    <option
                      key={group.providerId}
                      value={group.providerId}
                      disabled={!group.available}
                    >
                      {group.label}
                    </option>
                  ))}
                </select>
              )}
              {/* 模型 select：可伸缩 + 省略显示长名；title 保留全名（可访问全名）。
                  不用 truncate 类：原生 select 上的 overflow-hidden 会连下拉箭头一起
                  裁掉（Chrome 实测），浏览器的文本裁剪已足够，长名靠 title + option
                  原文取全。 */}
              <select
                data-testid="zcode-model-select"
                aria-label={i18n.t("mobile.zcodeControl.modelAria")}
                title={currentChoice?.modelId ?? selection.model}
                value={currentChoice?.modelId ?? ""}
                disabled={!selectionEnabled || activeModels.length === 0}
                onChange={(e) => handleModelChange(e.target.value)}
                className="max-w-full min-w-[4rem] flex-1 rounded-lg border border-[var(--cb)] px-2 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
              >
                {activeModels.map((choice) => (
                  <option key={choice.modelId} value={choice.modelId}>
                    {choice.modelId}
                  </option>
                ))}
              </select>
              {/* 档位 select：内容短，固定不收缩（长档位名靠 title 兜底）。 */}
              <select
                data-testid="zcode-thought-select"
                aria-label={i18n.t("mobile.zcodeControl.thoughtAria")}
                title={currentThought}
                value={currentThought}
                disabled={!selectionEnabled}
                onChange={(e) => handleThoughtChange(e.target.value)}
                className="max-w-[45%] shrink-0 rounded-lg border border-[var(--cb)] px-2 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
              >
                {(currentChoice?.thoughtLevels ?? []).map((level) => (
                  <option key={level} value={level}>
                    {level}
                  </option>
                ))}
              </select>
            </>
          )}
        </div>
      )}
      {selection !== null && !selectionEnabled && (
        <p
          data-testid="zcode-selection-reason"
          data-reason-code={selectionReason}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {zcodeReasonText(selectionReason)}
        </p>
      )}
      {modeView !== null && (
        <div
          data-testid="zcode-mode-bar"
          className="mb-2 flex flex-wrap items-center gap-2 text-xs text-[var(--tx)]"
        >
          <span className="text-[11px] text-[var(--mut)]">
            {i18n.t("mobile.zcodeControl.modeAria")}
          </span>
          <span data-testid="zcode-mode-current" data-mode={effectiveMode ?? "unknown"}>
            {effectiveMode === null
              ? i18n.t("mobile.zcodeControl.modeReason")
              : i18n.t(modeLabels[effectiveMode])}
          </span>
          <select
            data-testid="zcode-mode-select"
            aria-label={i18n.t("mobile.zcodeControl.modeAria")}
            value={pendingMode}
            disabled={!modeEnabled}
            onChange={(e) => setPendingMode(e.target.value as CollaborationMode | "")}
            className="rounded-lg border border-[var(--cb)] px-2 py-1 text-xs text-[var(--tx)] disabled:opacity-40"
          >
            {pendingMode === "" && <option value="">—</option>}
            {(["plan", "build", "guarded", "yolo"] as const).map((mode) => (
              <option key={mode} value={mode}>
                {i18n.t(modeLabels[mode])}
              </option>
            ))}
            {effectiveMode === "edit" && (
              <option value="edit" disabled>
                {i18n.t(modeLabels.edit)}
              </option>
            )}
          </select>
          <button
            type="button"
            data-testid="zcode-mode-apply"
            disabled={!modeEnabled || pendingMode === "" || pendingMode === effectiveMode}
            onClick={applyMode}
            className="rounded-full border border-[var(--cb)] px-2 py-1 text-xs text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t("mobile.zcodeControl.modeApply")}
          </button>
          {!modeEnabled && modeView !== null && (
            <span
              data-testid="zcode-mode-reason"
              data-reason-code={session?.capabilities.mode?.reasonCode ?? "not_ready"}
              className="text-[11px] text-[var(--mut)]"
            >
              {zcodeReasonText(session?.capabilities.mode?.reasonCode ?? "not_ready")}
            </span>
          )}
        </div>
      )}
      {modeConfirmOpen && (
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="zcode-mode-confirm-title"
          data-testid="zcode-mode-confirm-dialog"
          className="mb-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2"
        >
          <p id="zcode-mode-confirm-title" className="text-xs font-medium text-[var(--tx)]">
            {i18n.t("mobile.zcodeControl.modeConfirmTitle")}
          </p>
          <p className="mt-1 text-[11px] leading-4 text-[var(--mut)]">
            {i18n.t("mobile.zcodeControl.modeConfirmHint")}
          </p>
          <div className="mt-2 flex flex-wrap gap-2">
            <button
              type="button"
              data-testid="zcode-mode-confirm"
              onClick={() => {
                setModeConfirmOpen(false);
                submitMode("yolo");
              }}
              className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.modeConfirm")}
            </button>
            <button
              type="button"
              data-testid="zcode-mode-cancel"
              onClick={() => {
                setModeConfirmOpen(false);
                setPendingMode(effectiveMode ?? "");
              }}
              className="rounded-lg px-3 py-1.5 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.zcodeControl.modeCancel")}
            </button>
          </div>
        </div>
      )}
      {attachments.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {attachments.map((a) => (
            <span
              key={a.id}
              data-testid="zcode-attachment"
              data-status={a.status}
              title={a.error ?? a.path ?? undefined}
              className="max-w-full truncate rounded-md border border-[var(--cb)] px-2 py-1 text-xs text-[var(--tx)]"
            >
              {a.isImage ? "🖼" : "📎"} {a.name}
              {a.status === "uploading" && (
                <span className="ml-1 text-[var(--mut)]">
                  {typeof a.loaded === "number"
                    ? ` ${a.loaded}${a.total ? `/${a.total}` : ""}B`
                    : ""}
                </span>
              )}
              {a.status === "failed" && a.error && (
                <span className="ml-1 text-red-500">{a.error}</span>
              )}
              <button
                type="button"
                data-testid="zcode-attachment-remove"
                aria-label={i18n.t("mobile.zcodeControl.removeAttachment")}
                onClick={() => removeZcodeAttachment(a.id)}
                className="ml-1 text-[var(--mut)]"
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}
      {attachNotice !== null && (
        <p data-testid="zcode-attach-notice" className="text-xs text-amber-500">
          {i18n.t(
            attachNotice === "uploading"
              ? "mobile.zcodeControl.attachBlockedUploading"
              : "mobile.zcodeControl.attachBlockedFailed"
          )}
        </p>
      )}
      <div className="flex items-end gap-2">
        {/* 附件入口：＋选文件 + ？知情披露（存储说明）。与 MessageComposer 同款竖排
            （＋在上、？在下）：横向位紧张时竖排更省宽（2026-10-04 用户裁决口径）。
            ？只切换说明，**不**碰文件选择器 / 上传 / 发送（问号不是动作入口）。 */}
        <span className="flex shrink-0 flex-col items-center gap-0.5">
          <input
            ref={attachInputRef}
            data-testid="zcode-attachment-input"
            type="file"
            multiple
            className="hidden"
            onChange={(e) => {
              const files = Array.from(e.target.files ?? []);
              e.target.value = "";
              if (files.length > 0) addZcodeFiles(files);
            }}
          />
          <button
            type="button"
            data-testid="zcode-attach"
            aria-label={i18n.t("mobile.zcodeControl.attachAria")}
            onClick={() => attachInputRef.current?.click()}
            className="shrink-0 rounded-full bg-[var(--btnp)]/10 px-3 py-2 text-sm text-[var(--tx)]"
          >
            +
          </button>
          <button
            type="button"
            data-testid="zcode-attach-help"
            aria-label={i18n.t("mobile.composer.attachHelpAria")}
            aria-expanded={attachHintOpen}
            // aria-controls 只在说明真的渲染时给出：指向未渲染元素 = 悬空引用
            // （与 ZcodePendingCard 的 detail 展开同一口径）。
            aria-controls={attachHintOpen ? ATTACH_HINT_ID : undefined}
            onClick={() => setAttachHintOpen((open) => !open)}
            className="shrink-0 rounded-full p-0.5 text-[10px] leading-none text-[var(--mut)] hover:bg-[var(--cb)]"
          >
            ?
          </button>
        </span>
        <textarea
          ref={draftInputRef}
          data-testid="zcode-draft"
          aria-label={i18n.t("mobile.zcodeControl.draftAria")}
          value={draft}
          rows={2}
          onChange={(e) => updateDraft(e.target.value)}
          onPaste={handleZcodePaste}
          placeholder={i18n.t("mobile.zcodeControl.draftPlaceholder")}
          className="min-h-0 flex-1 resize-none rounded-lg border border-[var(--cb)] px-3 py-2 text-sm text-[var(--tx)] placeholder:text-[var(--mut)] focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none"
        />
        <button
          type="button"
          data-testid="zcode-send"
          // 主按钮唯一动作（send / stop / disabled）：由纯函数推导，UI 不另做判断。
          data-primary-action={primaryAction}
          // 主按钮所处的语义位（stop = 运行中且无内容 → 此按钮代表打断；send = 发送）。
          // 与 action 分离：位可以「是打断但此刻禁用」（例如多目标歧义 / 投影未就绪）。
          data-primary-mode={stopSlot ? "stop" : "send"}
          disabled={primaryAction === "disabled"}
          onClick={() => void handlePrimaryAction()}
          className="shrink-0 rounded-full bg-[var(--btnp)]/10 px-4 py-2 text-sm text-[var(--tx)] disabled:opacity-40"
        >
          {i18n.t(
            stopSlot
              ? "mobile.zcodeControl.interrupt"
              : submitting
                ? "mobile.zcodeControl.sending"
                : "mobile.zcodeControl.send"
          )}
        </button>
      </div>
      {/* 主按钮旁的说明（删除独立打断行后，原因不再消失）：
          - `zcode-send-reason`：冻结码原因（为什么不发/不打断）；
          - `zcode-primary-stop-hint`：打断位说明（运行中 → 此按钮将打断当前回合）。 */}
      {primaryReason !== null && (
        <p
          data-testid="zcode-send-reason"
          data-reason-code={primaryReason}
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {zcodeReasonText(primaryReason)}
        </p>
      )}
      {primaryAction === "stop" && (
        <p data-testid="zcode-primary-stop-hint" className="mt-1 text-xs text-[var(--mut)]">
          {i18n.t("mobile.zcodeControl.primaryStopHint")}
        </p>
      )}
      {/* 附件存储说明（知情披露）：？点开才显示，看过即收、不平铺常驻。
          文案**复用** MessageComposer 的既有说明（同一上传管线 → 同一落盘位置），
          不另写一份存储说明，也不改存储目录。 */}
      {attachHintOpen && (
        <p
          id={ATTACH_HINT_ID}
          data-testid="zcode-attach-hint"
          className="mt-1 rounded-lg bg-[var(--cbg)] px-2 py-1.5 text-[11px] leading-4 text-[var(--mut)]"
        >
          {i18n.t("mobile.composer.attachHint")}
        </p>
      )}
      {heldQueueChanged && (
        <p data-testid="zcode-held-queue-stale" className="mt-2 text-xs text-[var(--mut)]">
          {i18n.t("mobile.zcodeControl.heldQueueChanged")}
        </p>
      )}
      {heldQueuePrompt && (
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="zcode-held-queue-title"
          data-testid="zcode-held-queue-dialog"
          className="mt-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2"
        >
          <p id="zcode-held-queue-title" className="text-xs font-medium text-[var(--tx)]">
            {i18n.t("mobile.zcodeControl.heldQueueTitle")}
          </p>
          <p className="mt-1 text-[11px] leading-4 text-[var(--mut)]">
            {i18n.t("mobile.zcodeControl.heldQueueHint")}
          </p>
          <div className="mt-2 flex flex-wrap gap-2">
            <button
              ref={heldQueueKeepRef}
              type="button"
              data-testid="zcode-held-queue-keep"
              onClick={() => confirmHeldQueue("keepQueueAndSend")}
              className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.heldQueueKeep")}
            </button>
            <button
              type="button"
              data-testid="zcode-held-queue-clear"
              onClick={() => confirmHeldQueue("clearQueueAndSend")}
              className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.heldQueueClear")}
            </button>
            <button
              type="button"
              data-testid="zcode-held-queue-cancel"
              onClick={() => {
                setHeldQueuePrompt(false);
                setHeldQueueChanged(false);
              }}
              className="rounded-lg px-3 py-1.5 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.zcodeControl.heldQueueCancel")}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
